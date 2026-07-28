//! Grok Build hook adapter for Atomic Agent.
//!
//! Grok fires Claude-compatible lifecycle hooks with a camelCase JSON
//! envelope on stdin. Installation is handled by the standalone
//! `atomic-grok` package (hooks file + skills + rules); this adapter only
//! parses events from the installed hooks.
//!
//! # Hook Events
//!
//! | Grok event         | CLI verb              | HookType     |
//! |--------------------|-----------------------|--------------|
//! | `SessionStart`     | `session-start`       | SessionStart |
//! | `SessionEnd`       | `session-end`         | SessionEnd   |
//! | `UserPromptSubmit` | `user-prompt-submit`  | TurnStart    |
//! | `Stop`             | `stop`                | TurnEnd      |
//! | `PreToolUse`       | `pre-tool`            | PreToolUse   |
//! | `PostToolUse`      | `post-tool`           | PostToolUse  |
//!
//! # JSON shape
//!
//! Grok's stdin envelope uses camelCase throughout (`sessionId`, `toolName`,
//! `toolResult`, …). Claude Code compatibility aliases are accepted via
//! serde `alias` so shared scripts still parse. Every event also carries
//! `cwd` / `workspaceRoot` which we surface as repo-root hints when the
//! process cwd is not the workspace.
//!
//! `Stop` fires both on genuine turn completion (`reason: "end_turn"`) and
//! once more at session end (`channel_closed` / `shutdown`). Both map to
//! `TurnEnd`; empty working-copy diffs make the second fire a no-op.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// User-level Grok config directory name under `$HOME`.
const GROK_DIR: &str = ".grok";

/// Hooks file installed by the `atomic-grok` package.
const ATOMIC_HOOKS_FILE: &str = "atomic.json";

/// Substring identifying Atomic hooks in Grok settings / hook files.
const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks grok";

// JSON input types (camelCase primary, snake_case aliases)

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CommonInput {
    // Grok primary: camelCase. Aliases accept Claude-style snake_case scripts.
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default, alias = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "workspaceRoot")]
    workspace_root: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default, alias = "toolName")]
    tool_name: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
    #[serde(default, alias = "toolInput")]
    tool_input: Option<Value>,
    #[serde(default, alias = "toolResult", alias = "tool_response")]
    tool_result: Option<Value>,
    /// Stop reason: `"end_turn"`, `"channel_closed"`, `"shutdown"`, …
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, alias = "stopHookActive")]
    stop_hook_active: Option<bool>,
    #[serde(default, alias = "lastAssistantMessage")]
    last_assistant_message: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, alias = "permissionMode")]
    permission_mode: Option<String>,
    #[serde(default, alias = "hookEventName")]
    hook_event_name: Option<String>,
}

// GrokHook

/// Grok Build agent hook adapter.
///
/// Parses camelCase (and snake_case) hook JSON. Installation is managed by
/// the `atomic-grok` package via `atomic agent enable --agent grok`.
#[derive(Debug)]
pub struct GrokHook {
    _private: (),
}

impl GrokHook {
    /// Create a new Grok Build hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Path to the global Atomic hooks file (`~/.grok/hooks/atomic.json`).
    fn global_hooks_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(GROK_DIR).join("hooks").join(ATOMIC_HOOKS_FILE))
    }

    /// Whether the `atomic-grok` package has installed hooks globally.
    fn is_hooks_installed() -> bool {
        Self::global_hooks_path()
            .map(|path| {
                std::fs::read_to_string(path)
                    .map(|content| content.contains(ATOMIC_HOOK_PREFIX))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("grok-{}", uuid_short()))
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

    fn parse_common(&self, hook_type: HookType, raw: Value) -> AgentResult<CommonInput> {
        serde_json::from_value(raw).map_err(|e| AgentError::HookParseFailed {
            agent: self.name().to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })
    }
}

impl Default for GrokHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for GrokHook {
    fn name(&self) -> &str {
        "grok"
    }

    fn display_name(&self) -> &str {
        "Grok Build"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        let raw_json = self.parse_json(hook_type, input)?;
        let parsed = self.parse_common(hook_type, raw_json.clone())?;

        let mut raw_json = with_xai_provider(raw_json);
        if hook_type == HookType::TurnEnd {
            raw_json = normalize_stop_raw(raw_json, parsed.reason.as_deref());
        }
        if hook_type == HookType::PostToolUse {
            raw_json = normalize_tool_raw(raw_json, parsed.tool_result.as_ref());
        }

        // Promote model/provider into raw_json when present (orchestrator reads them).
        if let Some(model) = parsed.model.as_ref() {
            insert_if_missing(&mut raw_json, "model", Value::String(model.clone()));
        }
        if let Some(provider) = parsed.provider.as_ref() {
            insert_if_missing(&mut raw_json, "provider", Value::String(provider.clone()));
        }

        let mut event = TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
            .with_raw_json(raw_json);

        if let Some(path) = parsed.transcript_path.filter(|p| !p.is_empty()) {
            event = event.with_transcript_path(path);
        }

        match hook_type {
            HookType::TurnStart => {
                if let Some(prompt) = parsed.prompt.filter(|p| !p.is_empty()) {
                    event = event.with_prompt(prompt);
                }
            }
            HookType::PreToolUse | HookType::PostToolUse => {
                if let Some(name) = parsed.tool_name.filter(|n| !n.is_empty()) {
                    event = event.with_tool_name(name);
                }
                if let Some(id) = parsed.tool_use_id.filter(|id| !id.is_empty()) {
                    event = event.with_tool_use_id(id);
                }
            }
            HookType::SessionStart | HookType::SessionEnd | HookType::TurnEnd => {}
        }

        Ok(event)
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        // Installation is handled by the atomic-grok package via the
        // integrations engine. Report 1 if hooks are already present.
        if Self::is_hooks_installed() {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        // Uninstall is receipt-driven by `atomic agent disable`.
        Ok(())
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        Self::is_hooks_installed()
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
        repo_root.join(GROK_DIR).is_dir()
            || Self::global_hooks_path().is_some_and(|p| p.exists())
            || dirs::home_dir().is_some_and(|h| h.join(GROK_DIR).is_dir())
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "user-prompt-submit",
            "stop",
            "pre-tool",
            "post-tool",
        ]
    }

    fn repo_root_hints(&self, event: &TurnEvent) -> Option<Vec<PathBuf>> {
        let raw = event.raw_json.as_ref()?;
        let mut paths = Vec::new();
        for key in ["workspaceRoot", "workspace_root", "cwd"] {
            if let Some(p) = raw
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let path = PathBuf::from(p);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

// Normalization helpers

fn with_xai_provider(mut raw: Value) -> Value {
    if let Some(obj) = raw.as_object_mut() {
        obj.entry("provider".to_string())
            .or_insert_with(|| Value::String("xai".to_string()));
    }
    raw
}

/// Map Grok's `reason` onto the `finish_reason` field used by the record pipeline.
fn normalize_stop_raw(mut raw: Value, reason: Option<&str>) -> Value {
    let finish = match reason {
        Some("end_turn") | None => "stop",
        Some("channel_closed") | Some("shutdown") => "session-end",
        Some(other) => other,
    };
    if let Some(obj) = raw.as_object_mut() {
        obj.entry("finish_reason".to_string())
            .or_insert_with(|| Value::String(finish.to_string()));
        // Also mirror snake_case aliases the orchestrator may look up.
        if let Some(active) = obj.get("stopHookActive").cloned() {
            obj.entry("stop_hook_active".to_string()).or_insert(active);
        }
        if let Some(msg) = obj.get("lastAssistantMessage").cloned() {
            obj.entry("last_assistant_message".to_string())
                .or_insert(msg);
        }
    }
    raw
}

fn normalize_tool_raw(mut raw: Value, tool_result: Option<&Value>) -> Value {
    // Mirror camelCase tool fields into the snake_case keys downstream expects.
    if let Some(obj) = raw.as_object_mut() {
        if let Some(name) = obj.get("toolName").cloned() {
            obj.entry("tool_name".to_string()).or_insert(name);
        }
        if let Some(id) = obj.get("toolUseId").cloned() {
            obj.entry("tool_use_id".to_string()).or_insert(id);
        }
        if let Some(input) = obj.get("toolInput").cloned() {
            obj.entry("tool_input".to_string()).or_insert(input);
        }
    }

    let result = tool_result
        .cloned()
        .or_else(|| raw.get("toolResult").cloned())
        .or_else(|| raw.get("tool_response").cloned());

    if let Some(response) = result {
        if let Some(output) = extract_tool_output(&response) {
            insert_if_missing(&mut raw, "tool_output", Value::String(output));
        }
        if response.get("error").is_some() {
            insert_if_missing(&mut raw, "status", Value::String("error".to_string()));
        } else {
            insert_if_missing(&mut raw, "status", Value::String("completed".to_string()));
        }
        // Keep a snake_case alias of the result for shared tooling.
        insert_if_missing(&mut raw, "tool_response", response);
    }

    // File path from tool input when present.
    if let Some(path) = extract_file_path(raw.get("toolInput").or_else(|| raw.get("tool_input"))) {
        insert_if_missing(&mut raw, "file_path", Value::String(path));
    }

    raw
}

fn extract_tool_output(response: &Value) -> Option<String> {
    if let Some(text) = response.as_str() {
        return Some(text.to_string());
    }
    for key in ["output", "content", "result", "stdout", "message"] {
        if let Some(text) = response.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    // Fall back to compact JSON when the result is structured.
    serde_json::to_string(response).ok()
}

fn extract_file_path(tool_input: Option<&Value>) -> Option<String> {
    let value = tool_input?;
    for key in ["file_path", "filePath", "path", "target_file"] {
        if let Some(path) = value.get(key).and_then(Value::as_str) {
            return Some(path.to_string());
        }
    }
    None
}

fn insert_if_missing(raw: &mut Value, key: &str, value: Value) {
    if let Some(obj) = raw.as_object_mut() {
        obj.entry(key.to_string()).or_insert(value);
    }
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;

    fn make_hook() -> GrokHook {
        GrokHook::new()
    }

    #[test]
    fn test_name_and_display() {
        let hook = make_hook();
        assert_eq!(hook.name(), "grok");
        assert_eq!(hook.display_name(), "Grok Build");
    }

    #[test]
    fn test_supported_hooks_and_verbs() {
        let hook = make_hook();
        assert_eq!(hook.supported_hooks().len(), 6);
        assert_eq!(hook.hook_verbs().len(), 6);
        assert!(hook.hook_verbs().contains(&"session-start"));
        assert!(hook.hook_verbs().contains(&"session-end"));
        assert!(hook.hook_verbs().contains(&"user-prompt-submit"));
        assert!(hook.hook_verbs().contains(&"stop"));
        assert!(hook.hook_verbs().contains(&"pre-tool"));
        assert!(hook.hook_verbs().contains(&"post-tool"));
    }

    #[test]
    fn test_parse_session_start_camel_case() {
        let hook = make_hook();
        let input = br#"{
            "hookEventName": "session_start",
            "sessionId": "abc-123",
            "cwd": "/Users/me/project",
            "workspaceRoot": "/Users/me/project",
            "timestamp": "2026-04-14T12:00:00Z",
            "permissionMode": "default"
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "abc-123");
        assert_eq!(event.event_type, HookType::SessionStart);
        let raw = event.raw_json.as_ref().unwrap();
        assert_eq!(raw.get("provider").and_then(Value::as_str), Some("xai"));
    }

    #[test]
    fn test_parse_session_start_snake_case_alias() {
        let hook = make_hook();
        let input = br#"{"session_id": "s-snake", "cwd": "/tmp"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s-snake");
    }

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"sessionId": "abc-123", "reason": "shutdown"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "abc-123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    #[test]
    fn test_parse_user_prompt_submit() {
        let hook = make_hook();
        let input = br#"{
            "sessionId": "s1",
            "prompt": "Add retry logic to the client",
            "cwd": "/repo"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(
            event.prompt.as_deref(),
            Some("Add retry logic to the client")
        );
    }

    #[test]
    fn test_parse_stop_end_turn() {
        let hook = make_hook();
        let input = br#"{
            "sessionId": "s1",
            "reason": "end_turn",
            "stopHookActive": false,
            "lastAssistantMessage": "Done."
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::TurnEnd);
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("stop")
        );
        assert_eq!(
            raw.get("stop_hook_active").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            raw.get("last_assistant_message").and_then(Value::as_str),
            Some("Done.")
        );
    }

    #[test]
    fn test_parse_stop_session_end_fire() {
        let hook = make_hook();
        let input = br#"{"sessionId": "s1", "reason": "channel_closed"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("session-end")
        );
    }

    #[test]
    fn test_parse_pre_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "sessionId": "s1",
            "toolName": "run_terminal_command",
            "toolUseId": "tu-1",
            "toolInput": {"command": "cargo test"},
            "cwd": "/repo"
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("run_terminal_command"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tu-1"));
    }

    #[test]
    fn test_parse_post_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "sessionId": "s1",
            "toolName": "search_replace",
            "toolUseId": "tu-2",
            "toolInput": {"target_file": "src/main.rs"},
            "toolResult": "ok"
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("search_replace"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tu-2"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("tool_output").and_then(Value::as_str), Some("ok"));
        assert_eq!(raw.get("status").and_then(Value::as_str), Some("completed"));
        assert_eq!(
            raw.get("file_path").and_then(Value::as_str),
            Some("src/main.rs")
        );
        assert_eq!(raw.get("provider").and_then(Value::as_str), Some("xai"));
    }

    #[test]
    fn test_parse_empty_input() {
        let hook = make_hook();
        let err = hook.parse_event(HookType::TurnEnd, b"").unwrap_err();
        assert!(matches!(err, AgentError::HookInputEmpty { .. }));
    }

    #[test]
    fn test_parse_invalid_json() {
        let hook = make_hook();
        let err = hook
            .parse_event(HookType::TurnEnd, b"not-json")
            .unwrap_err();
        assert!(matches!(err, AgentError::HookParseFailed { .. }));
    }

    #[test]
    fn test_missing_session_id_fallback() {
        let hook = make_hook();
        let event = hook
            .parse_event(HookType::SessionStart, br#"{"cwd":"/tmp"}"#)
            .unwrap();
        assert!(event.session_id.starts_with("grok-"));
    }

    #[test]
    fn test_repo_root_hints_from_workspace() {
        let hook = make_hook();
        let input = br#"{
            "sessionId": "s1",
            "cwd": "/Users/me/project/subdir",
            "workspaceRoot": "/Users/me/project"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let hints = hook.repo_root_hints(&event).unwrap();
        assert_eq!(
            hints,
            vec![
                PathBuf::from("/Users/me/project"),
                PathBuf::from("/Users/me/project/subdir"),
            ]
        );
    }
}

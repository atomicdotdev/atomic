//! Codex hook adapter for Atomic Agent.
//!
//! Codex reads hooks from `~/.codex/hooks.json`. The installed Atomic hooks
//! call `atomic agent hooks codex <verb>` with JSON on stdin.
//!
//! Supported verbs:
//!
//! | Codex hook       | CLI verb              | HookType       |
//! |------------------|-----------------------|----------------|
//! | `SessionStart`   | `session-start`       | SessionStart   |
//! | `UserPromptSubmit` | `user-prompt-submit` | TurnStart      |
//! | `Stop`           | `stop`                | TurnEnd        |
//! | `PreToolUse`     | `pre-tool`            | PreToolUse     |
//! | `PostToolUse`    | `post-tool`           | PostToolUse    |

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

use super::AgentHook;

const CODEX_DIR: &str = ".codex";
const HOOKS_FILE: &str = "hooks.json";
const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks codex";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptSubmitInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    #[serde(default)]
    stop_hook_active: Option<bool>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ToolUseInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
    #[serde(default)]
    tool_response: Option<Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CodexHooksFile {
    #[serde(default)]
    hooks: Map<String, Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug)]
pub struct CodexHook {
    _private: (),
}

impl CodexHook {
    pub fn new() -> Self {
        Self { _private: () }
    }

    pub fn global_hooks_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(CODEX_DIR).join(HOOKS_FILE))
    }

    pub fn install_global(&self, force: bool) -> AgentResult<usize> {
        let path = Self::global_hooks_path().ok_or_else(|| AgentError::ConfigError {
            operation: "resolve".to_string(),
            path: PathBuf::from("~/.codex/hooks.json"),
            reason: "Could not determine home directory for Codex hooks".to_string(),
        })?;
        install_hooks_at(&path, force)
    }

    pub fn uninstall_global(&self) -> AgentResult<()> {
        if let Some(path) = Self::global_hooks_path() {
            uninstall_hooks_at(&path)?;
        }
        Ok(())
    }

    pub fn is_installed_global(&self) -> bool {
        Self::global_hooks_path().is_some_and(|path| hooks_file_has_atomic_hooks(&path))
    }

    fn local_hooks_path(repo_root: &Path) -> PathBuf {
        repo_root.join(CODEX_DIR).join(HOOKS_FILE)
    }

    fn extract_session_id(
        session_id: Option<String>,
        thread_id: Option<String>,
        raw: &Value,
    ) -> String {
        session_id
            .or(thread_id)
            .or_else(|| value_string(raw, "conversation_id"))
            .or_else(|| value_string(raw, "thread_id"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("codex-{}", uuid_short()))
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

    fn parse_value<T: for<'de> Deserialize<'de>>(
        &self,
        hook_type: HookType,
        raw_json: Value,
    ) -> AgentResult<T> {
        serde_json::from_value(raw_json).map_err(|e| AgentError::HookParseFailed {
            agent: self.name().to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })
    }
}

impl Default for CodexHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for CodexHook {
    fn name(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        let raw_json = self.parse_json(hook_type, input)?;

        match hook_type {
            HookType::SessionStart => {
                let parsed: SessionStartInput = self.parse_value(hook_type, raw_json.clone())?;
                let mut event = TurnEvent::new(
                    Self::extract_session_id(parsed.session_id, parsed.thread_id, &raw_json),
                    hook_type,
                )
                .with_raw_json(with_openai_provider(raw_json));
                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                Ok(event)
            }
            HookType::TurnStart => {
                let parsed: UserPromptSubmitInput =
                    self.parse_value(hook_type, raw_json.clone())?;
                let mut event = TurnEvent::new(
                    Self::extract_session_id(parsed.session_id, parsed.thread_id, &raw_json),
                    hook_type,
                )
                .with_raw_json(with_openai_provider(raw_json));
                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(prompt) = parsed.prompt.filter(|p| !p.is_empty()) {
                    event = event.with_prompt(prompt);
                }
                Ok(event)
            }
            HookType::TurnEnd => {
                let parsed: StopInput = self.parse_value(hook_type, raw_json.clone())?;
                let raw_json = normalize_stop_raw(raw_json);
                let mut event = TurnEvent::new(
                    Self::extract_session_id(parsed.session_id, parsed.thread_id, &raw_json),
                    hook_type,
                )
                .with_raw_json(with_openai_provider(raw_json));
                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                Ok(event)
            }
            HookType::PreToolUse | HookType::PostToolUse => {
                let parsed: ToolUseInput = self.parse_value(hook_type, raw_json.clone())?;
                let raw_json = if hook_type == HookType::PostToolUse {
                    normalize_tool_raw(raw_json)
                } else {
                    raw_json
                };
                let mut event = TurnEvent::new(
                    Self::extract_session_id(parsed.session_id, parsed.thread_id, &raw_json),
                    hook_type,
                )
                .with_raw_json(with_openai_provider(raw_json));
                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(name) = parsed.tool_name.filter(|n| !n.is_empty()) {
                    event = event.with_tool_name(name);
                }
                let tool_use_id = parsed
                    .tool_use_id
                    .or(parsed.tool_call_id)
                    .or(parsed.call_id)
                    .filter(|id| !id.is_empty());
                if let Some(id) = tool_use_id {
                    event = event.with_tool_use_id(id);
                }
                Ok(event)
            }
            HookType::SessionEnd => Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: "Codex does not currently emit SessionEnd hooks".to_string(),
            }),
        }
    }

    fn install(&self, repo_root: &Path) -> AgentResult<usize> {
        install_hooks_at(&Self::local_hooks_path(repo_root), false)
    }

    fn uninstall(&self, repo_root: &Path) -> AgentResult<()> {
        uninstall_hooks_at(&Self::local_hooks_path(repo_root))
    }

    fn is_installed(&self, repo_root: &Path) -> bool {
        hooks_file_has_atomic_hooks(&Self::local_hooks_path(repo_root))
            || self.is_installed_global()
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
            || Self::global_hooks_path().is_some_and(|path| path.exists())
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "user-prompt-submit",
            "stop",
            "pre-tool",
            "post-tool",
        ]
    }
}

pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "user-prompt-submit" => Some(HookType::TurnStart),
        "stop" => Some(HookType::TurnEnd),
        "pre-tool" => Some(HookType::PreToolUse),
        "post-tool" => Some(HookType::PostToolUse),
        _ => None,
    }
}

fn install_hooks_at(path: &Path, force: bool) -> AgentResult<usize> {
    let mut config = read_hooks_file(path)?;
    if force {
        remove_atomic_hooks(&mut config.hooks);
    }

    let mut installed = 0;
    for spec in CODEX_HOOK_DEFS {
        if add_hook(
            &mut config.hooks,
            spec.event,
            spec.command,
            spec.status_message,
        ) {
            installed += 1;
        }
    }

    write_hooks_file(path, &config)?;
    Ok(installed)
}

fn uninstall_hooks_at(path: &Path) -> AgentResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut config = read_hooks_file(path)?;
    remove_atomic_hooks(&mut config.hooks);
    write_hooks_file(path, &config)
}

fn read_hooks_file(path: &Path) -> AgentResult<CodexHooksFile> {
    if !path.exists() {
        return Ok(CodexHooksFile::default());
    }
    let content = std::fs::read_to_string(path).map_err(|e| AgentError::ConfigError {
        operation: "read".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    serde_json::from_str(&content).map_err(|e| AgentError::ConfigError {
        operation: "parse".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn write_hooks_file(path: &Path, config: &CodexHooksFile) -> AgentResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
            operation: "create directory".to_string(),
            path: parent.to_path_buf(),
            reason: e.to_string(),
        })?;
    }
    let content = serde_json::to_string_pretty(config).map_err(|e| AgentError::ConfigError {
        operation: "serialize".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    std::fs::write(path, content).map_err(|e| AgentError::ConfigError {
        operation: "write".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn hooks_file_has_atomic_hooks(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| content.contains(ATOMIC_HOOK_PREFIX))
        .unwrap_or(false)
}

fn add_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    status_message: Option<&str>,
) -> bool {
    let groups = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(groups) = groups.as_array_mut() else {
        *groups = Value::Array(Vec::new());
        let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
            return false;
        };
        return add_hook_to_groups(groups, command, status_message);
    };
    add_hook_to_groups(groups, command, status_message)
}

fn add_hook_to_groups(
    groups: &mut Vec<Value>,
    command: &str,
    status_message: Option<&str>,
) -> bool {
    if groups.iter().any(|group| group_has_command(group, command)) {
        return false;
    }

    let mut entry = Map::new();
    entry.insert("type".to_string(), Value::String("command".to_string()));
    entry.insert("command".to_string(), Value::String(command.to_string()));
    if let Some(message) = status_message {
        entry.insert(
            "statusMessage".to_string(),
            Value::String(message.to_string()),
        );
    }

    let mut group = Map::new();
    group.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(entry)]),
    );
    groups.push(Value::Object(group));
    true
}

fn group_has_command(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| cmd == command)
            })
        })
}

fn remove_atomic_hooks(hooks: &mut Map<String, Value>) {
    for value in hooks.values_mut() {
        let Some(groups) = value.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(group_obj) = group.as_object_mut() else {
                return true;
            };
            let Some(group_hooks) = group_obj.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            group_hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_atomic_hook)
            });
            !group_hooks.is_empty()
        });
    }
}

fn is_atomic_hook(command: &str) -> bool {
    command.contains(ATOMIC_HOOK_PREFIX)
}

fn with_openai_provider(mut raw: Value) -> Value {
    if let Some(obj) = raw.as_object_mut() {
        obj.entry("provider".to_string())
            .or_insert_with(|| Value::String("openai".to_string()));
    }
    raw
}

fn normalize_stop_raw(mut raw: Value) -> Value {
    let stop_hook_active = raw.get("stop_hook_active").and_then(Value::as_bool);
    if let Some(active) = stop_hook_active {
        if let Some(obj) = raw.as_object_mut() {
            obj.entry("finish_reason".to_string()).or_insert_with(|| {
                Value::String(if active { "tool-calls" } else { "stop" }.to_string())
            });
        }
    }
    raw
}

fn normalize_tool_raw(mut raw: Value) -> Value {
    let Some(response) = raw.get("tool_response").cloned() else {
        return raw;
    };

    if let Some(output) = extract_tool_output(&response) {
        insert_if_missing(&mut raw, "tool_output", Value::String(output));
    }
    if let Some(status) = extract_tool_status(&response) {
        insert_if_missing(&mut raw, "status", Value::String(status));
    }
    if let Some(duration) = extract_duration_ms(&response) {
        insert_if_missing(&mut raw, "duration", Value::Number(duration.into()));
    }
    if let Some(file_path) = extract_file_path(raw.get("tool_input"), &response) {
        insert_if_missing(&mut raw, "file_path", Value::String(file_path));
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
    None
}

fn extract_tool_status(response: &Value) -> Option<String> {
    if response
        .get("success")
        .and_then(Value::as_bool)
        .is_some_and(|success| success)
    {
        return Some("completed".to_string());
    }
    if response
        .get("success")
        .and_then(Value::as_bool)
        .is_some_and(|success| !success)
        || response.get("error").is_some()
    {
        return Some("error".to_string());
    }
    None
}

fn extract_duration_ms(response: &Value) -> Option<u64> {
    response
        .get("duration_ms")
        .or_else(|| response.get("duration"))
        .and_then(Value::as_u64)
}

fn extract_file_path(tool_input: Option<&Value>, response: &Value) -> Option<String> {
    for value in [tool_input, Some(response)].into_iter().flatten() {
        for key in ["file_path", "filePath", "path"] {
            if let Some(path) = value.get(key).and_then(Value::as_str) {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn insert_if_missing(raw: &mut Value, key: &str, value: Value) {
    if let Some(obj) = raw.as_object_mut() {
        obj.entry(key.to_string()).or_insert(value);
    }
}

fn value_string(raw: &Value, key: &str) -> Option<String> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

struct HookDef {
    event: &'static str,
    command: &'static str,
    status_message: Option<&'static str>,
}

const CODEX_HOOK_DEFS: &[HookDef] = &[
    HookDef {
        event: "SessionStart",
        command: "test -d .atomic && atomic agent hooks codex session-start || true",
        status_message: Some("Atomic: tracking session"),
    },
    HookDef {
        event: "UserPromptSubmit",
        command: "test -d .atomic && atomic agent hooks codex user-prompt-submit || true",
        status_message: None,
    },
    HookDef {
        event: "Stop",
        command: "test -d .atomic && atomic agent hooks codex stop || true",
        status_message: None,
    },
    HookDef {
        event: "PreToolUse",
        command: "test -d .atomic && atomic agent hooks codex pre-tool || true",
        status_message: None,
    },
    HookDef {
        event: "PostToolUse",
        command: "test -d .atomic && atomic agent hooks codex post-tool || true",
        status_message: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;
    use tempfile::TempDir;

    fn make_hook() -> CodexHook {
        CodexHook::new()
    }

    #[test]
    fn test_name() {
        assert_eq!(make_hook().name(), "codex");
    }

    #[test]
    fn test_display_name() {
        assert_eq!(make_hook().display_name(), "Codex");
    }

    #[test]
    fn test_default() {
        assert_eq!(CodexHook::default().name(), "codex");
    }

    #[test]
    fn test_supported_hooks() {
        let hooks = make_hook().supported_hooks();
        assert_eq!(hooks.len(), 5);
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::PreToolUse));
        assert!(hooks.contains(&HookType::PostToolUse));
        assert!(!hooks.contains(&HookType::SessionEnd));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(
            verbs,
            vec![
                "session-start",
                "user-prompt-submit",
                "stop",
                "pre-tool",
                "post-tool"
            ]
        );
    }

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
        assert_eq!(verb_to_hook_type("pre-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("session-end"), None);
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    #[test]
    fn test_parse_session_start() {
        let input = br#"{
            "session_id": "sess-123",
            "transcript_path": "/tmp/codex.jsonl",
            "model": "gpt-5.5",
            "source": "startup",
            "cwd": "/repo"
        }"#;
        let event = make_hook()
            .parse_event(HookType::SessionStart, input)
            .unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert_eq!(
            event.transcript_path.as_deref(),
            Some(Path::new("/tmp/codex.jsonl"))
        );
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "gpt-5.5");
        assert_eq!(raw["provider"], "openai");
    }

    #[test]
    fn test_parse_session_start_uses_thread_id_fallback() {
        let input = br#"{"thread_id": "thread-123"}"#;
        let event = make_hook()
            .parse_event(HookType::SessionStart, input)
            .unwrap();
        assert_eq!(event.session_id, "thread-123");
    }

    #[test]
    fn test_parse_session_start_generates_fallback_session_id() {
        let input = br#"{"cwd": "/repo"}"#;
        let event = make_hook()
            .parse_event(HookType::SessionStart, input)
            .unwrap();
        assert!(event.session_id.starts_with("codex-"));
    }

    #[test]
    fn test_parse_user_prompt_submit() {
        let input = br#"{
            "session_id": "sess-123",
            "prompt": "fix the hook",
            "model": "gpt-5.5"
        }"#;
        let event = make_hook().parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("fix the hook"));
        assert_eq!(event.raw_json.unwrap()["provider"], "openai");
    }

    #[test]
    fn test_parse_user_prompt_submit_empty_prompt_is_none() {
        let input = br#"{"session_id": "sess-123", "prompt": ""}"#;
        let event = make_hook().parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_stop_preserves_assistant_summary_and_finish_reason() {
        let input = br#"{
            "session_id": "sess-123",
            "last_assistant_message": "Updated the parser and tests.",
            "stop_hook_active": false,
            "model": "gpt-5.5"
        }"#;
        let event = make_hook().parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnEnd);
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw["last_assistant_message"],
            "Updated the parser and tests."
        );
        assert_eq!(raw["finish_reason"], "stop");
    }

    #[test]
    fn test_parse_stop_active_infers_tool_calls_finish_reason() {
        let input = br#"{"session_id": "sess-123", "stop_hook_active": true}"#;
        let event = make_hook().parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.raw_json.unwrap()["finish_reason"], "tool-calls");
    }

    #[test]
    fn test_parse_pre_tool() {
        let input = br#"{
            "session_id": "sess-123",
            "tool_name": "exec_command",
            "tool_use_id": "call-1",
            "tool_input": {"cmd": "cargo test"}
        }"#;
        let event = make_hook()
            .parse_event(HookType::PreToolUse, input)
            .unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-1"));
        assert_eq!(event.raw_json.unwrap()["tool_input"]["cmd"], "cargo test");
    }

    #[test]
    fn test_parse_pre_tool_accepts_tool_call_id_alias() {
        let input = br#"{
            "session_id": "sess-123",
            "tool_name": "exec_command",
            "tool_call_id": "call-2"
        }"#;
        let event = make_hook()
            .parse_event(HookType::PreToolUse, input)
            .unwrap();
        assert_eq!(event.tool_use_id.as_deref(), Some("call-2"));
    }

    #[test]
    fn test_parse_post_tool_normalizes_tool_response_for_provenance() {
        let input = br#"{
            "session_id": "sess-123",
            "tool_name": "exec_command",
            "tool_use_id": "call-3",
            "tool_input": {"cmd": "cargo test", "file_path": "src/lib.rs"},
            "tool_response": {"output": "ok", "success": true, "duration_ms": 42}
        }"#;
        let event = make_hook()
            .parse_event(HookType::PostToolUse, input)
            .unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("exec_command"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-3"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["tool_output"], "ok");
        assert_eq!(raw["status"], "completed");
        assert_eq!(raw["duration"], 42);
        assert_eq!(raw["file_path"], "src/lib.rs");
    }

    #[test]
    fn test_parse_post_tool_normalizes_error_response() {
        let input = br#"{
            "session_id": "sess-123",
            "tool_name": "exec_command",
            "tool_response": {"error": "failed"}
        }"#;
        let event = make_hook()
            .parse_event(HookType::PostToolUse, input)
            .unwrap();
        assert_eq!(event.raw_json.unwrap()["status"], "error");
    }

    #[test]
    fn test_parse_session_end_is_unsupported() {
        let result = make_hook().parse_event(HookType::SessionEnd, br#"{"session_id":"s"}"#);
        assert!(matches!(result, Err(AgentError::HookParseFailed { .. })));
    }

    #[test]
    fn test_parse_event_empty_input() {
        let result = make_hook().parse_event(HookType::TurnEnd, b"");
        assert!(matches!(result, Err(AgentError::HookInputEmpty { .. })));
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let result = make_hook().parse_event(HookType::TurnEnd, b"not-json");
        assert!(matches!(result, Err(AgentError::HookParseFailed { .. })));
    }

    #[test]
    fn test_detect_presence_with_local_codex_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(CODEX_DIR)).unwrap();
        assert!(make_hook().detect_presence(dir.path()));
    }

    #[test]
    fn test_local_install_is_idempotent_and_preserves_other_hooks() {
        let dir = TempDir::new().unwrap();
        let hooks_path = dir.path().join(CODEX_DIR).join(HOOKS_FILE);
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"custom stop"}]}]},"other":true}"#,
        )
        .unwrap();

        let hook = make_hook();
        assert_eq!(hook.install(dir.path()).unwrap(), 5);
        assert!(hook.is_installed(dir.path()));
        assert_eq!(hook.install(dir.path()).unwrap(), 0);

        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(content.contains("custom stop"));
        assert!(content.contains("atomic agent hooks codex session-start"));
        assert!(content.contains("atomic agent hooks codex user-prompt-submit"));
        assert!(content.contains("atomic agent hooks codex stop"));
        assert!(content.contains("atomic agent hooks codex pre-tool"));
        assert!(content.contains("atomic agent hooks codex post-tool"));
        assert!(content.contains("\"other\": true"));
    }

    #[test]
    fn test_uninstall_removes_only_atomic_hooks() {
        let dir = TempDir::new().unwrap();
        let hook = make_hook();
        hook.install(dir.path()).unwrap();
        let hooks_path = dir.path().join(CODEX_DIR).join(HOOKS_FILE);
        let mut config = read_hooks_file(&hooks_path).unwrap();
        add_hook_to_groups(
            config
                .hooks
                .get_mut("Stop")
                .and_then(Value::as_array_mut)
                .unwrap(),
            "custom stop",
            None,
        );
        write_hooks_file(&hooks_path, &config).unwrap();

        hook.uninstall(dir.path()).unwrap();
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(!content.contains("atomic agent hooks codex"));
        assert!(content.contains("custom stop"));
    }

    #[test]
    fn test_uninstall_missing_file_is_ok() {
        let dir = TempDir::new().unwrap();
        make_hook().uninstall(dir.path()).unwrap();
    }

    #[test]
    fn test_force_install_rewrites_atomic_hooks() {
        let dir = TempDir::new().unwrap();
        let hooks_path = dir.path().join(CODEX_DIR).join(HOOKS_FILE);
        install_hooks_at(&hooks_path, false).unwrap();
        assert_eq!(install_hooks_at(&hooks_path, true).unwrap(), 5);
    }
}

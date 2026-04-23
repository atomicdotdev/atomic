//! Cline hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing and presence detection via the `.clinerules/` directory.
//! Cline hooks are individual executable script files (not a JSON config), but the
//! Rust adapter only needs to parse JSON from stdin.
//!
//! # Cline Hooks Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Cline Hooks (~/.clinerules/hooks/ or ~/Documents/Cline/Hooks/)         │
//! │                                                                         │
//! │  TaskStart        ──▶  atomic agent hooks cline task-start             │
//! │  TaskResume       ──▶  atomic agent hooks cline task-resume            │
//! │  TaskComplete     ──▶  atomic agent hooks cline task-complete          │
//! │  TaskCancel       ──▶  atomic agent hooks cline task-cancel            │
//! │  UserPromptSubmit ──▶  atomic agent hooks cline user-prompt            │
//! │  PostToolUse      ──▶  atomic agent hooks cline post-tool              │
//! │  PreToolUse       ──▶  atomic agent hooks cline pre-tool               │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook System
//!
//! Cline hooks are script files (extensionless on macOS/Linux, `.ps1` on Windows)
//! placed in either:
//! - **Global**: `~/Documents/Cline/Hooks/`
//! - **Project**: `.clinerules/hooks/`
//!
//! Each hook receives JSON on stdin with common fields (`taskId`, `hookName`,
//! `clineVersion`, `timestamp`, `workspaceRoots`, `userId`, `model`) plus
//! hook-specific nested data. The hook returns JSON on stdout with `cancel`,
//! `contextModification`, and `errorMessage` fields.
//!
//! # Hook Verbs
//!
//! | Verb            | HookType       | Description                             |
//! |-----------------|----------------|-----------------------------------------|
//! | `task-start`    | SessionStart   | New Cline task begins                    |
//! | `task-resume`   | SessionStart   | Interrupted task resumed                 |
//! | `task-complete` | TurnEnd        | Task finishes — triggers recording       |
//! | `task-cancel`   | SessionEnd     | Task cancelled                           |
//! | `user-prompt`   | TurnStart      | User sends a message                     |
//! | `post-tool`     | PostToolUse    | After tool execution                     |
//! | `pre-tool`      | PreToolUse     | Before tool execution                    |
//!
//! # Differences from Other Adapters
//!
//! | Aspect          | Copilot / Claude Code        | Cline                         |
//! |-----------------|------------------------------|-------------------------------|
//! | Config dir      | `.github/` / `.claude/`      | `.clinerules/`                |
//! | Hook format     | JSON config file             | Executable script files       |
//! | Session ID      | `session_id`                 | `taskId`                      |
//! | Model info      | Flat `model` string          | Nested `model.provider/slug`  |
//! | Event names     | camelCase / PascalCase       | PascalCase event names        |
//! | Tool names      | Mixed                        | snake_case                    |
//! | Hook data       | Flat fields                  | Nested per-event objects      |

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// =============================================================================
// Constants
// =============================================================================

/// The `.clinerules` directory at the repo root.
const CLINERULES_DIR: &str = ".clinerules";

// =============================================================================
// Cline JSON Input Types
// =============================================================================

/// Nested model information from Cline.
///
/// Cline sends model info as a nested object with `provider` and `slug`
/// fields, unlike other adapters that send a flat model string.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClineModel {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

/// Data nested under `taskStart` or `taskResume` or `taskCancel` or `taskComplete`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TaskStartData {
    #[serde(default)]
    task: Option<String>,
}

/// Data nested under `userPromptSubmit`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptData {
    #[serde(default)]
    prompt: Option<String>,
}

/// Data nested under `preToolUse`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreToolUseData {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

/// Data nested under `postToolUse`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseData {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default, alias = "durationMs")]
    duration_ms: Option<u64>,
}

/// Full Cline hook input with common fields and all possible nested data.
///
/// Cline sends a single JSON object on stdin. Common fields are top-level,
/// and hook-specific data lives in a nested object named after the hook type
/// (only one is present per event).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClineInput {
    // Common fields
    #[serde(default, alias = "taskId")]
    task_id: Option<String>,
    #[serde(default, alias = "hookName")]
    hook_name: Option<String>,
    #[serde(default, alias = "clineVersion")]
    cline_version: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default, alias = "workspaceRoots")]
    workspace_roots: Option<Vec<String>>,
    #[serde(default, alias = "userId")]
    user_id: Option<String>,
    #[serde(default)]
    model: Option<ClineModel>,

    // Hook-specific nested fields (only one is present per event)
    #[serde(default, alias = "taskStart")]
    task_start: Option<TaskStartData>,
    #[serde(default, alias = "taskResume")]
    task_resume: Option<TaskStartData>,
    #[serde(default, alias = "taskCancel")]
    task_cancel: Option<TaskStartData>,
    #[serde(default, alias = "taskComplete")]
    task_complete: Option<TaskStartData>,
    #[serde(default, alias = "userPromptSubmit")]
    user_prompt_submit: Option<UserPromptData>,
    #[serde(default, alias = "preToolUse")]
    pre_tool_use: Option<PreToolUseData>,
    #[serde(default, alias = "postToolUse")]
    post_tool_use: Option<PostToolUseData>,
}

// =============================================================================
// ClineHook
// =============================================================================

/// Cline hook adapter.
///
/// Cline uses executable script files for hooks rather than a JSON config file.
/// Installation and uninstallation are managed by the `atomic-cline` sub-project,
/// so the Rust adapter only handles JSON parsing from stdin and presence detection.
///
/// # Differences from Other Adapters
///
/// | Aspect          | Copilot / Claude Code      | Cline                       |
/// |-----------------|---------------------------|-----------------------------|
/// | Config location | `.github/` / `.claude/`    | `.clinerules/`              |
/// | Session ID key  | `session_id`               | `taskId`                    |
/// | Model format    | Flat string                | Nested `{provider, slug}`   |
/// | Hook data       | Flat top-level fields      | Nested per-event objects    |
/// | Tool names      | Mixed casing               | snake_case                  |
#[derive(Debug)]
pub struct ClineHook {
    _private: (),
}

impl ClineHook {
    /// Create a new Cline hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from Cline's `taskId`, generating a fallback if missing.
    fn extract_session_id(task_id: Option<String>) -> String {
        task_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("cline-{}", uuid_short()))
    }
}

impl Default for ClineHook {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// AgentHook Trait Implementation
// =============================================================================

impl AgentHook for ClineHook {
    fn name(&self) -> &str {
        "cline"
    }

    fn display_name(&self) -> &str {
        "Cline"
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

        let parsed: ClineInput =
            serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        // Extract model info from the nested model object.
        // Store slug as the model name and provider as the vendor.
        let model_slug = parsed
            .model
            .as_ref()
            .and_then(|m| m.slug.as_deref())
            .map(|s| s.to_string());
        let model_provider = parsed
            .model
            .as_ref()
            .and_then(|m| m.provider.as_deref())
            .map(|s| s.to_string());

        match hook_type {
            HookType::SessionStart => {
                // task-start or task-resume
                let prompt = parsed
                    .task_start
                    .as_ref()
                    .and_then(|d| d.task.clone())
                    .or_else(|| parsed.task_resume.as_ref().and_then(|d| d.task.clone()));

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(p) = prompt {
                    event = event.with_prompt(p);
                }

                // Store model info in raw_json for the orchestrator
                if let Some(ref mut raw) = event.raw_json {
                    if let Some(obj) = raw.as_object_mut() {
                        if let Some(ref slug) = model_slug {
                            obj.insert(
                                "model_name".to_string(),
                                serde_json::Value::String(slug.clone()),
                            );
                        }
                        if let Some(ref provider) = model_provider {
                            obj.insert(
                                "vendor".to_string(),
                                serde_json::Value::String(provider.clone()),
                            );
                        }
                    }
                }

                Ok(event)
            }

            HookType::TurnEnd => {
                // task-complete — triggers recording
                let prompt = parsed.task_complete.as_ref().and_then(|d| d.task.clone());

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(p) = prompt {
                    event = event.with_prompt(p);
                }

                Ok(event)
            }

            HookType::SessionEnd => {
                // task-cancel
                let prompt = parsed.task_cancel.as_ref().and_then(|d| d.task.clone());

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(p) = prompt {
                    event = event.with_prompt(p);
                }

                Ok(event)
            }

            HookType::TurnStart => {
                // user-prompt
                let prompt = parsed
                    .user_prompt_submit
                    .as_ref()
                    .and_then(|d| d.prompt.clone());

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(p) = prompt {
                    event = event.with_prompt(p);
                }

                // Store model info for the orchestrator
                if let Some(ref mut raw) = event.raw_json {
                    if let Some(obj) = raw.as_object_mut() {
                        if let Some(ref slug) = model_slug {
                            obj.insert(
                                "model_name".to_string(),
                                serde_json::Value::String(slug.clone()),
                            );
                        }
                        if let Some(ref provider) = model_provider {
                            obj.insert(
                                "vendor".to_string(),
                                serde_json::Value::String(provider.clone()),
                            );
                        }
                    }
                }

                Ok(event)
            }

            HookType::PreToolUse => {
                let tool_data = parsed.pre_tool_use.as_ref();

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(data) = tool_data {
                    if let Some(ref name) = data.tool {
                        event = event.with_tool_name(name.clone());
                    }
                }

                Ok(event)
            }

            HookType::PostToolUse => {
                let tool_data = parsed.post_tool_use.as_ref();

                let mut event = TurnEvent::new(Self::extract_session_id(parsed.task_id), hook_type)
                    .with_raw_json(raw_json);

                if let Some(data) = tool_data {
                    if let Some(ref name) = data.tool {
                        event = event.with_tool_name(name.clone());
                    }
                    // Store duration and result in raw_json for downstream use
                    if let Some(duration) = data.duration_ms {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "duration_ms".to_string(),
                                    serde_json::Value::Number(duration.into()),
                                );
                            }
                        }
                    }
                    if let Some(ref result) = data.result {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "tool_result".to_string(),
                                    serde_json::Value::String(result.clone()),
                                );
                            }
                        }
                    }
                }

                Ok(event)
            }
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-cline project
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-cline project
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-cline project
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
        repo_root.join(CLINERULES_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "task-start",
            "task-resume",
            "task-complete",
            "task-cancel",
            "user-prompt",
            "post-tool",
            "pre-tool",
        ]
    }
}

// =============================================================================
// Verb Mapping
// =============================================================================

/// Convert a Cline-specific verb to a [`HookType`].
///
/// | Verb            | HookType     |
/// |-----------------|--------------|
/// | `task-start`    | SessionStart |
/// | `task-resume`   | SessionStart |
/// | `task-complete` | TurnEnd      |
/// | `task-cancel`   | SessionEnd   |
/// | `user-prompt`   | TurnStart    |
/// | `post-tool`     | PostToolUse  |
/// | `pre-tool`      | PreToolUse   |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "task-start" | "task-resume" => Some(HookType::SessionStart),
        "task-complete" => Some(HookType::TurnEnd),
        "task-cancel" => Some(HookType::SessionEnd),
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
/// Only used when Cline fails to provide a `taskId`, which should be
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

    fn make_hook() -> ClineHook {
        ClineHook::new()
    }

    // ── Basic trait method tests ────────────────────────────────────

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "cline");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Cline");
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
    fn test_supported_hooks_count() {
        let hook = make_hook();
        assert_eq!(hook.supported_hooks().len(), 6);
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 7);
        assert!(verbs.contains(&"task-start"));
        assert!(verbs.contains(&"task-resume"));
        assert!(verbs.contains(&"task-complete"));
        assert!(verbs.contains(&"task-cancel"));
        assert!(verbs.contains(&"user-prompt"));
        assert!(verbs.contains(&"post-tool"));
        assert!(verbs.contains(&"pre-tool"));
    }

    #[test]
    fn test_default() {
        let hook = ClineHook::default();
        assert_eq!(hook.name(), "cline");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("ClineHook"));
    }

    // ── parse_event tests: task-start (SessionStart) ────────────────

    #[test]
    fn test_parse_task_start() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "abc123",
            "hookName": "TaskStart",
            "timestamp": "1736654400000",
            "workspaceRoots": ["/path/to/project"],
            "model": {"provider": "openrouter", "slug": "anthropic/claude-sonnet-4.5"},
            "taskStart": {"task": "Add authentication to the API"}
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "abc123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Add authentication to the API")
        );
    }

    #[test]
    fn test_parse_task_start_with_model_info() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "t1",
            "model": {"provider": "openrouter", "slug": "anthropic/claude-sonnet-4.5"},
            "taskStart": {"task": "Fix bug"}
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model_name"], "anthropic/claude-sonnet-4.5");
        assert_eq!(raw["vendor"], "openrouter");
    }

    #[test]
    fn test_parse_task_start_camel_case_task_id() {
        let hook = make_hook();
        let input = br#"{"taskId": "task-camel-123"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "task-camel-123");
    }

    #[test]
    fn test_parse_task_start_snake_case_task_id() {
        let hook = make_hook();
        let input = br#"{"task_id": "task-snake-456"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "task-snake-456");
    }

    #[test]
    fn test_parse_task_start_missing_task_id() {
        let hook = make_hook();
        let input = br#"{"hookName": "TaskStart"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("cline-"));
    }

    #[test]
    fn test_parse_task_start_empty_task_id() {
        let hook = make_hook();
        let input = br#"{"taskId": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("cline-"));
    }

    #[test]
    fn test_parse_task_start_no_nested_data() {
        let hook = make_hook();
        let input = br#"{"taskId": "t1"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "t1");
        assert!(event.prompt.is_none());
    }

    // ── parse_event tests: task-resume (SessionStart) ───────────────

    #[test]
    fn test_parse_task_resume() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "resume-123",
            "hookName": "TaskResume",
            "taskResume": {"task": "Continue the API work"}
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "resume-123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert_eq!(event.prompt.as_deref(), Some("Continue the API work"));
    }

    // ── parse_event tests: task-complete (TurnEnd) ──────────────────

    #[test]
    fn test_parse_task_complete() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "done-456",
            "hookName": "TaskComplete",
            "taskComplete": {"task": "Authentication implemented"}
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "done-456");
        assert_eq!(event.event_type, HookType::TurnEnd);
        assert_eq!(event.prompt.as_deref(), Some("Authentication implemented"));
    }

    #[test]
    fn test_parse_task_complete_no_nested_data() {
        let hook = make_hook();
        let input = br#"{"taskId": "done-789"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "done-789");
        assert!(event.prompt.is_none());
    }

    // ── parse_event tests: task-cancel (SessionEnd) ─────────────────

    #[test]
    fn test_parse_task_cancel() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "cancel-111",
            "hookName": "TaskCancel",
            "taskCancel": {"task": "Cancelled by user"}
        }"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "cancel-111");
        assert_eq!(event.event_type, HookType::SessionEnd);
        assert_eq!(event.prompt.as_deref(), Some("Cancelled by user"));
    }

    #[test]
    fn test_parse_task_cancel_no_nested_data() {
        let hook = make_hook();
        let input = br#"{"taskId": "cancel-222"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "cancel-222");
        assert!(event.prompt.is_none());
    }

    // ── parse_event tests: user-prompt (TurnStart) ──────────────────

    #[test]
    fn test_parse_user_prompt() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "prompt-1",
            "hookName": "UserPromptSubmit",
            "userPromptSubmit": {"prompt": "Fix the auth bug in login.rs"}
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "prompt-1");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Fix the auth bug in login.rs")
        );
    }

    #[test]
    fn test_parse_user_prompt_snake_case_alias() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "prompt-2",
            "user_prompt_submit": {"prompt": "Refactor the database layer"}
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.prompt.as_deref(), Some("Refactor the database layer"));
    }

    #[test]
    fn test_parse_user_prompt_no_prompt() {
        let hook = make_hook();
        let input = br#"{"taskId": "prompt-3"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_user_prompt_with_model() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "prompt-4",
            "model": {"provider": "anthropic", "slug": "claude-sonnet-4-5"},
            "userPromptSubmit": {"prompt": "hello"}
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model_name"], "claude-sonnet-4-5");
        assert_eq!(raw["vendor"], "anthropic");
    }

    // ── parse_event tests: pre-tool (PreToolUse) ────────────────────

    #[test]
    fn test_parse_pre_tool() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "tool-1",
            "hookName": "PreToolUse",
            "preToolUse": {
                "tool": "write_to_file",
                "parameters": {"path": "src/main.rs", "content": "fn main() {}"}
            }
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "tool-1");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("write_to_file"));
    }

    #[test]
    fn test_parse_pre_tool_snake_case_alias() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "tool-2",
            "pre_tool_use": {"tool": "read_file"}
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn test_parse_pre_tool_minimal() {
        let hook = make_hook();
        let input = br#"{"taskId": "tool-3"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
    }

    #[test]
    fn test_parse_pre_tool_snake_case_tool_names() {
        let hook = make_hook();
        // Cline uses snake_case tool names
        let tools = vec![
            "read_file",
            "write_to_file",
            "execute_command",
            "search_files",
        ];
        for tool in tools {
            let input = format!(
                r#"{{"taskId": "t1", "preToolUse": {{"tool": "{}"}}}}"#,
                tool
            );
            let event = hook
                .parse_event(HookType::PreToolUse, input.as_bytes())
                .unwrap();
            assert_eq!(event.tool_name.as_deref(), Some(tool));
        }
    }

    // ── parse_event tests: post-tool (PostToolUse) ──────────────────

    #[test]
    fn test_parse_post_tool() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "tool-4",
            "hookName": "PostToolUse",
            "postToolUse": {
                "tool": "execute_command",
                "parameters": {"command": "cargo test"},
                "result": "All tests passed",
                "success": true,
                "durationMs": 2500
            }
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "tool-4");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("execute_command"));
        // Check duration and result stored in raw_json
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["duration_ms"], 2500);
        assert_eq!(raw["tool_result"], "All tests passed");
    }

    #[test]
    fn test_parse_post_tool_snake_case_alias() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "tool-5",
            "post_tool_use": {
                "tool": "write_to_file",
                "result": "File written",
                "success": true,
                "duration_ms": 100
            }
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("write_to_file"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["duration_ms"], 100);
        assert_eq!(raw["tool_result"], "File written");
    }

    #[test]
    fn test_parse_post_tool_no_tool_name() {
        let hook = make_hook();
        let input = br#"{"taskId": "tool-6"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
    }

    #[test]
    fn test_parse_post_tool_no_duration() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "tool-7",
            "postToolUse": {"tool": "read_file"}
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("read_file"));
        // duration_ms should not be present in raw_json if not provided
        let raw = event.raw_json.unwrap();
        assert!(raw.get("duration_ms").is_none());
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
        let input = br#"{"taskId": "t1", "unknown_field": 42, "another": "value"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "t1");
    }

    #[test]
    fn test_parse_minimal_input() {
        let hook = make_hook();
        let input = br#"{}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("cline-"));
    }

    // ── verb_to_hook_type tests ─────────────────────────────────────

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("task-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(
            verb_to_hook_type("task-resume"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("task-complete"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("task-cancel"), Some(HookType::SessionEnd));
        assert_eq!(verb_to_hook_type("user-prompt"), Some(HookType::TurnStart));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("pre-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
        assert_eq!(verb_to_hook_type("session-start"), None); // Not a Cline verb
    }

    // ── detect_presence tests ───────────────────────────────────────

    #[test]
    fn test_detect_presence_with_clinerules_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".clinerules")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_clinerules_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_file_not_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".clinerules"), "not a dir").unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    // ── Installation is a no-op (managed by atomic-cline project) ───

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
        let id = ClineHook::extract_session_id(Some("task-123".to_string()));
        assert_eq!(id, "task-123");
    }

    #[test]
    fn test_extract_session_id_empty_generates_fallback() {
        let id = ClineHook::extract_session_id(Some("".to_string()));
        assert!(id.starts_with("cline-"));
    }

    #[test]
    fn test_extract_session_id_none_generates_fallback() {
        let id = ClineHook::extract_session_id(None);
        assert!(id.starts_with("cline-"));
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

        let test_cases: Vec<(HookType, &[u8])> = vec![
            (
                HookType::SessionStart,
                br#"{"taskId":"t1","taskStart":{"task":"start"}}"#,
            ),
            (
                HookType::SessionEnd,
                br#"{"taskId":"t1","taskCancel":{"task":"cancelled"}}"#,
            ),
            (
                HookType::TurnStart,
                br#"{"taskId":"t1","userPromptSubmit":{"prompt":"fix bug"}}"#,
            ),
            (
                HookType::TurnEnd,
                br#"{"taskId":"t1","taskComplete":{"task":"done"}}"#,
            ),
            (
                HookType::PreToolUse,
                br#"{"taskId":"t1","preToolUse":{"tool":"read_file"}}"#,
            ),
            (
                HookType::PostToolUse,
                br#"{"taskId":"t1","postToolUse":{"tool":"write_to_file","durationMs":500}}"#,
            ),
        ];

        for (hook_type, input) in test_cases {
            let event = hook
                .parse_event(hook_type, input)
                .unwrap_or_else(|e| panic!("Failed to parse {:?}: {}", hook_type, e));
            assert_eq!(event.session_id, "t1");
            assert_eq!(event.event_type, hook_type);
        }
    }

    // ── Model extraction tests ──────────────────────────────────────

    #[test]
    fn test_parse_model_provider_only() {
        let hook = make_hook();
        let input = br#"{"taskId": "t1", "model": {"provider": "anthropic"}}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["vendor"], "anthropic");
        assert!(raw.get("model_name").is_none());
    }

    #[test]
    fn test_parse_model_slug_only() {
        let hook = make_hook();
        let input = br#"{"taskId": "t1", "model": {"slug": "gpt-4o"}}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model_name"], "gpt-4o");
        assert!(raw.get("vendor").is_none());
    }

    #[test]
    fn test_parse_model_not_present() {
        let hook = make_hook();
        let input = br#"{"taskId": "t1"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert!(raw.get("model_name").is_none());
        assert!(raw.get("vendor").is_none());
    }

    #[test]
    fn test_parse_model_empty_object() {
        let hook = make_hook();
        let input = br#"{"taskId": "t1", "model": {}}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert!(raw.get("model_name").is_none());
        assert!(raw.get("vendor").is_none());
    }

    // ── Full Cline payload test ─────────────────────────────────────

    #[test]
    fn test_parse_full_cline_payload() {
        let hook = make_hook();
        let input = br#"{
            "taskId": "abc123",
            "hookName": "TaskStart",
            "clineVersion": "3.2.1",
            "timestamp": "1736654400000",
            "workspaceRoots": ["/path/to/project"],
            "userId": "user-42",
            "model": {"provider": "openrouter", "slug": "anthropic/claude-sonnet-4.5"},
            "taskStart": {"task": "Add authentication to the API"}
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "abc123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Add authentication to the API")
        );
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model_name"], "anthropic/claude-sonnet-4.5");
        assert_eq!(raw["vendor"], "openrouter");
        // Original fields preserved
        assert_eq!(raw["hookName"], "TaskStart");
        assert_eq!(raw["clineVersion"], "3.2.1");
        assert_eq!(raw["userId"], "user-42");
    }

    #[test]
    fn test_debug_impl() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("ClineHook"));
    }
}

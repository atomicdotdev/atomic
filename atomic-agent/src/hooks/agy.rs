//! Antigravity CLI (`agy`) hook adapter for Atomic Agent.
//!
//! This module implements the [`AgentHook`] trait for Google's Antigravity
//! CLI — the successor to the deprecated Gemini CLI — handling:
//!
//! - **JSON parsing** of hook callbacks from stdin
//! - **Presence detection** via the `.agents/` directory
//!
//! # Installation Lives in the Integration Package
//!
//! This adapter does **not** install anything. The agy integration (plugin,
//! hooks, skills, and the `AGENTS.md` instruction file) is packaged in the
//! `atomic-agy` repository and installed by the integrations engine
//! (`crate::integrations`) when you run `atomic agent enable --agent agy`:
//! the CLI syncs the package from Atomic storage and installs it per its
//! `atomic-integration.toml`, staging the plugin at
//! `~/.gemini/config/plugins/atomic/` and registering it in agy's
//! `import_manifest.json`. `install()` below is only a presence detector so
//! `enable` can report status.
//!
//! # Why a Plugin, Not `.agents/hooks.json`
//!
//! Antigravity's docs describe project hooks in `.agents/hooks.json`, but in
//! CLI 1.1.4 a project-level file is only *loaded* (surfaced in the `/hooks`
//! panel) — its handlers never *fire*. Hooks delivered through the plugin
//! mechanism (`~/.gemini/config/plugins/<name>/hooks.json`) are registered
//! and executed.
//!
//! # Hook Execution Environment (Important)
//!
//! Antigravity runs plugin hooks with the **plugin directory** as the
//! working directory — not the workspace. A `test -d .atomic` shell guard
//! can therefore never work. Instead, the installed commands are bare
//! `atomic agent hooks agy <verb>` invocations, and the hook handler
//! resolves the repository from the `workspacePaths` field present in every
//! hook payload (see [`AgentHook::repo_root_hints`]).
//!
//! # Hook Events
//!
//! | Antigravity Hook | Atomic HookType | Key Input Fields                              |
//! |------------------|-----------------|-----------------------------------------------|
//! | `PreInvocation`  | TurnStart       | `conversationId`, `transcriptPath`, `invocationNum` |
//! | `Stop`           | TurnEnd         | `conversationId`, `terminationReason`, `fullyIdle`  |
//! | `PostToolUse`    | PostToolUse     | `conversationId`, `stepIdx`, `error`          |
//!
//! `PreToolUse` is intentionally **not** installed: Antigravity requires a
//! `decision` in the hook's stdout response (`allow`/`deny`/`ask`/`force_ask`)
//! and any value Atomic emitted would override the user's own permission
//! policy. `PostInvocation` duplicates `Stop` for provenance purposes and is
//! also skipped.
//!
//! `PreInvocation` fires before *every* model call (several per user prompt),
//! not once per prompt. The orchestrator tolerates repeated `TurnStart`
//! events on an active session, so this is harmless.
//!
//! # Stdout Contract
//!
//! Antigravity reads a JSON object from hook stdout. Atomic responds with
//! `{}` for every installed hook: it satisfies the `PostToolUse` contract
//! verbatim and is a no-op for `PreInvocation` (`injectSteps` optional) and
//! `Stop` (any `decision` other than `"continue"` — including a missing
//! field — lets the agent stop). See [`AgentHook::stdout_response`].
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_agent::hooks::agy::AgyHook;
//! use atomic_agent::hooks::AgentHook;
//! use atomic_agent::event::HookType;
//!
//! let hook = AgyHook::new();
//! assert_eq!(hook.name(), "agy");
//! assert_eq!(hook.display_name(), "Antigravity CLI");
//!
//! let input = br#"{"conversationId": "abc-123", "transcriptPath": "/tmp/t.jsonl", "workspacePaths": ["/repo"], "invocationNum": 0}"#;
//! let event = hook.parse_event(HookType::TurnStart, input).unwrap();
//! assert_eq!(event.session_id, "abc-123");
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// Constants

/// The plugin directory name under `~/.gemini/config/plugins/`.
const PLUGIN_DIR_NAME: &str = "atomic";

/// The hooks file inside the plugin directory.
const HOOKS_FILE: &str = "hooks.json";

/// The workspace customization directory used for presence detection.
///
/// agy reads workspace skills and MCP config from `.agents/`; users who
/// customize agy for a project create it.
const AGENTS_DIR: &str = ".agents";

// Antigravity JSON Input Types

/// JSON input for the `PreInvocation` hook (TurnStart).
///
/// Fires before each model invocation. `invocationNum` is 0 for the first
/// invocation of an execution loop.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreInvocationInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "invocationNum")]
    invocation_num: Option<u64>,
    #[serde(default, rename = "initialNumSteps")]
    initial_num_steps: Option<u64>,
}

/// JSON input for the `Stop` hook (TurnEnd).
///
/// Fires when the execution loop terminates. `fullyIdle` is `false` when
/// background tasks are still running.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "executionNum")]
    execution_num: Option<u64>,
    #[serde(default, rename = "terminationReason")]
    termination_reason: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "fullyIdle")]
    fully_idle: Option<bool>,
}

/// JSON input for the `PostToolUse` hook.
///
/// Fires after a tool completes. Antigravity does not include the tool name
/// or arguments in this payload — only the step index and an optional error
/// string. Richer detail must come from the transcript (`transcriptPath`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "stepIdx")]
    step_idx: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

// AgyHook

/// Antigravity CLI (`agy`) hook adapter for Atomic Agent.
///
/// Handles hook JSON parsing and presence detection. Installation is owned
/// by the integrations engine via the `atomic-agy` package — see the module
/// docs.
#[derive(Debug, Default)]
pub struct AgyHook {
    _private: (),
}

impl AgyHook {
    /// Create a new Antigravity CLI hook adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns agy's global config directory: `~/.gemini/config`.
    fn global_config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".gemini").join("config"))
    }

    /// Returns the plugin directory under the given config dir.
    fn plugin_dir(config_dir: &Path) -> PathBuf {
        config_dir.join("plugins").join(PLUGIN_DIR_NAME)
    }

    /// Returns the hooks file path under the given config dir.
    fn hooks_path(config_dir: &Path) -> PathBuf {
        Self::plugin_dir(config_dir).join(HOOKS_FILE)
    }

    /// Whether the integration's plugin is staged in agy's config.
    fn plugin_present() -> bool {
        Self::global_config_dir()
            .map(|dir| Self::hooks_path(&dir).exists())
            .unwrap_or(false)
    }

    /// Parse the common base fields and build a TurnEvent with the session
    /// ID and transcript path populated.
    fn base_event(
        &self,
        hook_type: HookType,
        conversation_id: Option<String>,
        transcript_path: Option<String>,
    ) -> TurnEvent {
        let session_id = conversation_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

        let mut event = TurnEvent::new(&session_id, hook_type);
        if let Some(path) = transcript_path {
            event = event.with_transcript_path(path);
        }
        event
    }
}

// AgentHook Implementation

impl AgentHook for AgyHook {
    fn name(&self) -> &str {
        "agy"
    }

    fn display_name(&self) -> &str {
        "Antigravity CLI"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        let raw_json: Value =
            serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        match hook_type {
            HookType::TurnStart => {
                // Antigravity: PreInvocation → TurnStart
                let parsed: PreInvocationInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                Ok(self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json))
            }

            HookType::TurnEnd => {
                // Antigravity: Stop → TurnEnd
                let parsed: StopInput = serde_json::from_value(raw_json.clone()).map_err(|e| {
                    AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    }
                })?;

                // Normalize the termination reason into the `finish_reason`
                // field the record pipeline understands (mirrors the Codex
                // adapter's stop normalization).
                let raw_json = normalize_stop_raw(raw_json);

                Ok(self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json))
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

                // Normalize `error` into the `status` field the provenance
                // accumulator reads. Antigravity sends no tool name here, so
                // the orchestrator records the call under "unknown" — the
                // transcript (whose path is on the event) holds the detail.
                let raw_json = normalize_tool_raw(raw_json);

                let mut event = self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json);

                // The step index is the closest thing to a tool call ID in
                // this payload — it is unique within the trajectory.
                if let Some(step) = parsed.step_idx {
                    event = event.with_tool_use_id(step.to_string());
                }

                Ok(event)
            }

            // Antigravity has no session lifecycle hooks; SessionStart /
            // SessionEnd / PreToolUse are never dispatched for this agent.
            _ => Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: format!("hook type {:?} is not supported by agy", hook_type),
            }),
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        // Installation is owned by the integrations engine (the atomic-agy
        // package on Atomic storage). Report 1 when the plugin is already
        // staged so `enable` shows a success message.
        if Self::plugin_present() {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation is receipt-driven via the integrations engine.
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        Self::plugin_present()
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        repo_root.join(AGENTS_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec!["pre-invocation", "stop", "post-tool-use"]
    }

    fn stdout_response(&self, _hook_type: HookType) -> Option<&'static str> {
        // Antigravity reads a JSON object from hook stdout. `{}` satisfies
        // the PostToolUse contract verbatim and is a no-op for PreInvocation
        // and Stop.
        Some("{}")
    }

    fn repo_root_hints(&self, event: &TurnEvent) -> Option<Vec<PathBuf>> {
        // Antigravity executes plugin hooks with the plugin directory as the
        // working directory, so the process cwd is useless for finding the
        // repository. Every hook payload carries the mounted workspaces.
        let paths = event
            .raw_json
            .as_ref()?
            .get("workspacePaths")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

// Raw JSON Normalization

/// Map Antigravity's `terminationReason` onto the `finish_reason` field used
/// by the record pipeline (mirrors the Codex adapter's stop normalization).
fn normalize_stop_raw(mut raw: Value) -> Value {
    let reason = raw
        .get("terminationReason")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    if let Some(reason) = reason {
        let finish_reason = match reason.as_str() {
            "model_stop" => "stop",
            "max_steps_exceeded" => "length",
            other => other,
        };
        if let Some(obj) = raw.as_object_mut() {
            obj.entry("finish_reason".to_string())
                .or_insert_with(|| Value::String(finish_reason.to_string()));
        }
    }

    raw
}

/// Map Antigravity's `error` field onto the `status` field the provenance
/// accumulator reads for tool calls.
fn normalize_tool_raw(mut raw: Value) -> Value {
    let has_error = raw
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty());

    if let Some(obj) = raw.as_object_mut() {
        obj.entry("status".to_string()).or_insert_with(|| {
            Value::String(if has_error { "error" } else { "completed" }.to_string())
        });
    }

    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> AgyHook {
        AgyHook::new()
    }

    // Identity tests

    #[test]
    fn test_name() {
        assert_eq!(make_hook().name(), "agy");
    }

    #[test]
    fn test_display_name() {
        assert_eq!(make_hook().display_name(), "Antigravity CLI");
    }

    #[test]
    fn test_supported_hooks() {
        let supported = make_hook().supported_hooks();
        assert!(supported.contains(&HookType::TurnStart));
        assert!(supported.contains(&HookType::TurnEnd));
        assert!(supported.contains(&HookType::PostToolUse));
        assert!(!supported.contains(&HookType::SessionStart));
        assert!(!supported.contains(&HookType::PreToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 3);
        assert!(verbs.contains(&"pre-invocation"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"post-tool-use"));
    }

    #[test]
    fn test_verbs_map_to_hook_types() {
        assert_eq!(
            HookType::from_verb("pre-invocation"),
            Some(HookType::TurnStart)
        );
        assert_eq!(HookType::from_verb("stop"), Some(HookType::TurnEnd));
        assert_eq!(
            HookType::from_verb("post-tool-use"),
            Some(HookType::PostToolUse)
        );
    }

    #[test]
    fn test_stdout_response() {
        let hook = make_hook();
        assert_eq!(hook.stdout_response(HookType::TurnStart), Some("{}"));
        assert_eq!(hook.stdout_response(HookType::TurnEnd), Some("{}"));
        assert_eq!(hook.stdout_response(HookType::PostToolUse), Some("{}"));
    }

    // Parse event tests

    #[test]
    fn test_parse_pre_invocation_turn_start() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "transcriptPath": "/home/u/.gemini/antigravity-cli/brain/ec33/logs/transcript.jsonl",
            "artifactDirectoryPath": "/home/u/.gemini/antigravity-cli/brain/ec33",
            "workspacePaths": ["/workspace/project"],
            "invocationNum": 0,
            "initialNumSteps": 0
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9-0cba-4100-8142-c61503f6c587");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert!(event.transcript_path.is_some());
    }

    #[test]
    fn test_parse_stop_turn_end() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9",
            "transcriptPath": "/tmp/transcript.jsonl",
            "workspacePaths": ["/workspace/project"],
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": "",
            "fullyIdle": true
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9");
        assert_eq!(event.event_type, HookType::TurnEnd);
        // terminationReason is normalized into finish_reason
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("stop")
        );
    }

    #[test]
    fn test_parse_stop_max_steps_maps_to_length() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1", "terminationReason": "max_steps_exceeded"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("length")
        );
    }

    #[test]
    fn test_parse_post_tool_use_success() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9",
            "transcriptPath": "/tmp/transcript.jsonl",
            "workspacePaths": ["/workspace/project"],
            "stepIdx": 5,
            "error": ""
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_use_id.as_deref(), Some("5"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("status").and_then(Value::as_str), Some("completed"));
    }

    #[test]
    fn test_parse_post_tool_use_error() {
        let hook = make_hook();
        let input = br#"{"conversationId": "ec33ebf9", "stepIdx": 7, "error": "exit status 1"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_use_id.as_deref(), Some("7"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("status").and_then(Value::as_str), Some("error"));
    }

    #[test]
    fn test_parse_missing_conversation_id_defaults_unknown() {
        let hook = make_hook();
        let input = br#"{"invocationNum": 0}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "unknown");
    }

    #[test]
    fn test_parse_empty_input() {
        let hook = make_hook();
        assert!(hook.parse_event(HookType::TurnStart, b"").is_err());
    }

    #[test]
    fn test_parse_invalid_json() {
        let hook = make_hook();
        assert!(hook.parse_event(HookType::TurnStart, b"not json").is_err());
    }

    #[test]
    fn test_parse_unsupported_hook_type() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1"}"#;
        assert!(hook.parse_event(HookType::SessionStart, input).is_err());
        assert!(hook.parse_event(HookType::PreToolUse, input).is_err());
    }

    // repo_root_hints tests

    #[test]
    fn test_repo_root_hints_from_workspace_paths() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "s1",
            "workspacePaths": ["/workspace/project", "/workspace/shared"]
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let hints = hook.repo_root_hints(&event).unwrap();
        assert_eq!(
            hints,
            vec![
                PathBuf::from("/workspace/project"),
                PathBuf::from("/workspace/shared")
            ]
        );
    }

    #[test]
    fn test_repo_root_hints_missing_field() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(hook.repo_root_hints(&event).is_none());
    }

    #[test]
    fn test_repo_root_hints_empty_array() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1", "workspacePaths": []}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(hook.repo_root_hints(&event).is_none());
    }

    // Detection tests

    #[test]
    fn test_detect_presence_with_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        assert!(make_hook().detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!make_hook().detect_presence(dir.path()));
    }
}

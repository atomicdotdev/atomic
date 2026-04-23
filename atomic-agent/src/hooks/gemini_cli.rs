//! Gemini CLI hook adapter for Atomic Agent.
//!
//! This module implements the [`AgentHook`] trait for Gemini CLI, handling:
//!
//! - **JSON parsing** of hook callbacks from stdin
//! - **Hook installation** into `.gemini/settings.json`
//! - **Hook removal** preserving non-Atomic hooks
//! - **Presence detection** via `.gemini/` directory
//!
//! # Gemini CLI Hook Architecture
//!
//! Gemini CLI fires hooks at specific lifecycle points. Each hook receives
//! JSON on stdin and can return JSON on stdout.
//!
//! | Gemini CLI Hook       | Atomic HookType    | Key Input Fields                           |
//! |-----------------------|--------------------|--------------------------------------------|
//! | `SessionStart`        | SessionStart       | `session_id`, `transcript_path`, `source`  |
//! | `SessionEnd`          | SessionEnd         | `session_id`, `transcript_path`, `reason`  |
//! | `BeforeAgent`         | TurnStart          | `session_id`, `transcript_path`, `prompt`  |
//! | `AfterAgent`          | TurnEnd            | `session_id`, `prompt`, `prompt_response`  |
//! | `BeforeTool`          | PreToolUse         | `session_id`, `tool_name`, `tool_input`    |
//! | `AfterTool`           | PostToolUse        | `session_id`, `tool_name`, `tool_response` |
//!
//! # Settings File Format
//!
//! Gemini CLI reads hooks from `.gemini/settings.json`:
//!
//! ```json
//! {
//!   "hooks": {
//!     "SessionStart": [
//!       {
//!         "matcher": "",
//!         "hooks": [
//!           {
//!             "type": "command",
//!             "command": "atomic agent hooks gemini-cli session-start",
//!             "name": "atomic-session-start"
//!           }
//!         ]
//!       }
//!     ],
//!     "AfterAgent": [
//!       {
//!         "matcher": "",
//!         "hooks": [
//!           {
//!             "type": "command",
//!             "command": "atomic agent hooks gemini-cli after-agent",
//!             "name": "atomic-after-agent"
//!           }
//!         ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! # Differences from Claude Code
//!
//! - Config lives in `.gemini/settings.json` (not `.claude/settings.json`)
//! - Turn start is `BeforeAgent` (not `user-prompt-submit`)
//! - Turn end is `AfterAgent` (not `stop`)
//! - Tool hooks use regex matchers on tool names (not specific tool name matchers)
//! - `SessionEnd` is **best-effort** — Gemini CLI will NOT wait for it to complete
//! - All hooks receive `session_id`, `transcript_path`, `cwd`, `timestamp` in base input
//! - Environment variables: `GEMINI_PROJECT_DIR`, `GEMINI_SESSION_ID`, `GEMINI_CWD`
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_agent::hooks::gemini_cli::GeminiCliHook;
//! use atomic_agent::hooks::AgentHook;
//! use atomic_agent::event::HookType;
//!
//! let hook = GeminiCliHook::new();
//! assert_eq!(hook.name(), "gemini-cli");
//! assert_eq!(hook.display_name(), "Gemini CLI");
//!
//! let input = br#"{"session_id": "abc-123", "transcript_path": "/tmp/t.json", "cwd": "/project", "hook_event_name": "SessionStart", "timestamp": "2026-01-01T00:00:00Z", "source": "startup"}"#;
//! let event = hook.parse_event(HookType::SessionStart, input).unwrap();
//! assert_eq!(event.session_id, "abc-123");
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// Constants

/// The directory name for Gemini CLI configuration.
const GEMINI_DIR: &str = ".gemini";

/// The settings file that Gemini CLI reads hooks from.
const SETTINGS_FILE: &str = "settings.json";

/// Command prefix used to identify Atomic hooks in the settings file.
const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks gemini-cli";

/// Permission deny rule to prevent Gemini from reading Atomic metadata.
#[allow(dead_code)]
const METADATA_DENY_RULE: &str = ".atomic/metadata/**";

// Gemini CLI JSON Input Types

/// Base input fields present in every Gemini CLI hook callback.
///
/// All hooks receive these fields via stdin JSON.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BaseInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for SessionStart hook.
///
/// Fires on application startup, resuming a session, or after `/clear`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    /// How the session started: "startup", "resume", "clear"
    #[serde(default)]
    source: Option<String>,
}

/// JSON input for SessionEnd hook.
///
/// Fires when the CLI exits or a session is cleared.
/// **Best effort** — Gemini CLI will NOT wait for this hook to complete.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    /// Why the session ended: "exit", "clear", "logout", "other"
    #[serde(default)]
    reason: Option<String>,
}

/// JSON input for BeforeAgent hook (TurnStart).
///
/// Fires after user submits a prompt, before the agent begins planning.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BeforeAgentInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

/// JSON input for AfterAgent hook (TurnEnd).
///
/// Fires once per turn after the model generates its final response.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AfterAgentInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    prompt_response: Option<String>,
    #[serde(default)]
    stop_hook_active: Option<bool>,
}

/// JSON input for BeforeTool hook (PreToolUse).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BeforeToolInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

/// JSON input for AfterTool hook (PostToolUse).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AfterToolInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_response: Option<serde_json::Value>,
}

// Gemini CLI Settings File Types

/// A single hook entry within a matcher group.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiHookEntry {
    #[serde(rename = "type")]
    hook_type: String,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// A matcher group containing one or more hook entries.
///
/// For lifecycle hooks the `matcher` is empty or "*".
/// For tool hooks the `matcher` is a regex matching tool names.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiHookMatcher {
    #[serde(default)]
    matcher: String,
    hooks: Vec<GeminiHookEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequential: Option<bool>,
}

/// The hooks section of `.gemini/settings.json`.
///
/// Gemini CLI uses PascalCase event names as keys.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct GeminiHooks {
    #[serde(
        default,
        rename = "SessionStart",
        skip_serializing_if = "Vec::is_empty"
    )]
    session_start: Vec<GeminiHookMatcher>,

    #[serde(default, rename = "SessionEnd", skip_serializing_if = "Vec::is_empty")]
    session_end: Vec<GeminiHookMatcher>,

    #[serde(default, rename = "BeforeAgent", skip_serializing_if = "Vec::is_empty")]
    before_agent: Vec<GeminiHookMatcher>,

    #[serde(default, rename = "AfterAgent", skip_serializing_if = "Vec::is_empty")]
    after_agent: Vec<GeminiHookMatcher>,

    #[serde(default, rename = "BeforeTool", skip_serializing_if = "Vec::is_empty")]
    before_tool: Vec<GeminiHookMatcher>,

    #[serde(default, rename = "AfterTool", skip_serializing_if = "Vec::is_empty")]
    after_tool: Vec<GeminiHookMatcher>,

    // Preserve unknown hook types (BeforeModel, AfterModel, etc.)
    #[serde(flatten)]
    other: HashMap<String, serde_json::Value>,
}

// GeminiCliHook

/// Gemini CLI hook adapter for Atomic Agent.
///
/// Handles hook JSON parsing, installation into `.gemini/settings.json`,
/// and presence detection via the `.gemini/` directory.
#[derive(Debug, Default)]
pub struct GeminiCliHook {
    _private: (),
}

impl GeminiCliHook {
    /// Create a new Gemini CLI hook adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the path to `.gemini/settings.json` relative to the repo root.
    fn settings_path(repo_root: &Path) -> PathBuf {
        repo_root.join(GEMINI_DIR).join(SETTINGS_FILE)
    }

    /// Returns the path to the global `~/.gemini/settings.json`.
    pub fn global_settings_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(GEMINI_DIR).join(SETTINGS_FILE))
    }

    /// Read and parse the existing `.gemini/settings.json`, if it exists.
    ///
    /// Returns `(raw_settings, hooks)` where `raw_settings` preserves unknown
    /// fields and `hooks` is the parsed hooks section.
    fn read_settings(
        settings_path: &Path,
    ) -> AgentResult<(serde_json::Map<String, serde_json::Value>, GeminiHooks)> {
        if !settings_path.exists() {
            return Ok((serde_json::Map::new(), GeminiHooks::default()));
        }

        let content =
            std::fs::read_to_string(settings_path).map_err(|e| AgentError::ConfigError {
                operation: "read".to_string(),
                path: settings_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&content)
            .map_err(|e| AgentError::ConfigError {
                operation: "parse".to_string(),
                path: settings_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        let hooks = if let Some(hooks_val) = raw.get("hooks") {
            serde_json::from_value(hooks_val.clone()).unwrap_or_default()
        } else {
            GeminiHooks::default()
        };

        Ok((raw, hooks))
    }

    /// Write settings back to `.gemini/settings.json`.
    fn write_settings(
        settings_path: &Path,
        raw: &serde_json::Map<String, serde_json::Value>,
    ) -> AgentResult<()> {
        // Ensure parent directory exists
        if let Some(parent) = settings_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
                    operation: "create directory".to_string(),
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
            }
        }

        let content = serde_json::to_string_pretty(raw).map_err(|e| AgentError::ConfigError {
            operation: "serialize".to_string(),
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;

        std::fs::write(settings_path, content.as_bytes()).map_err(|e| AgentError::ConfigError {
            operation: "write".to_string(),
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;

        Ok(())
    }

    /// Install hooks into a settings file (project or global).
    fn install_to(settings_path: &Path, force: bool) -> AgentResult<usize> {
        // Ensure parent directory exists
        if let Some(parent) = settings_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
                    operation: "create directory".to_string(),
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
            }
        }

        let (mut raw, mut hooks) = Self::read_settings(settings_path)?;

        if force {
            remove_atomic_hooks(&mut hooks.session_start);
            remove_atomic_hooks(&mut hooks.session_end);
            remove_atomic_hooks(&mut hooks.before_agent);
            remove_atomic_hooks(&mut hooks.after_agent);
            remove_atomic_hooks(&mut hooks.before_tool);
            remove_atomic_hooks(&mut hooks.after_tool);
        }

        // Define the hooks to install:
        // (settings event key, matcher, verb, name, description)
        let hook_defs: Vec<(&str, &str, &str, &str)> = vec![
            ("session_start", "", "session-start", "atomic-session-start"),
            ("session_end", "", "session-end", "atomic-session-end"),
            ("before_agent", "", "before-agent", "atomic-turn-start"),
            ("after_agent", "", "after-agent", "atomic-turn-end"),
            ("before_tool", ".*", "before-tool", "atomic-pre-tool"),
            ("after_tool", ".*", "after-tool", "atomic-post-tool"),
        ];

        let mut count = 0;
        for (event_key, matcher, verb, hook_name) in &hook_defs {
            let command = format!("test -d .atomic && {} {} || true", ATOMIC_HOOK_PREFIX, verb);

            let matchers = match *event_key {
                "session_start" => &mut hooks.session_start,
                "session_end" => &mut hooks.session_end,
                "before_agent" => &mut hooks.before_agent,
                "after_agent" => &mut hooks.after_agent,
                "before_tool" => &mut hooks.before_tool,
                "after_tool" => &mut hooks.after_tool,
                _ => continue,
            };

            if !hook_command_exists(matchers, matcher, &command) {
                add_hook_to_matcher(matchers, matcher, &command, Some(hook_name));
                count += 1;
            }
        }

        if count == 0 {
            return Ok(0);
        }

        // Serialize hooks back into raw settings
        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(settings_path, &raw)?;

        Ok(count)
    }

    /// Uninstall hooks from a settings file.
    fn uninstall_from(settings_path: &Path) -> AgentResult<()> {
        if !settings_path.exists() {
            return Ok(());
        }

        let (mut raw, mut hooks) = Self::read_settings(settings_path)?;

        remove_atomic_hooks(&mut hooks.session_start);
        remove_atomic_hooks(&mut hooks.session_end);
        remove_atomic_hooks(&mut hooks.before_agent);
        remove_atomic_hooks(&mut hooks.after_agent);
        remove_atomic_hooks(&mut hooks.before_tool);
        remove_atomic_hooks(&mut hooks.after_tool);

        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(settings_path, &raw)?;

        Ok(())
    }

    /// Check if Atomic hooks are installed in a settings file.
    fn is_installed_in(settings_path: &Path) -> bool {
        if !settings_path.exists() {
            return false;
        }

        match Self::read_settings(settings_path) {
            Ok((_, hooks)) => {
                has_any_atomic_hook(&hooks.session_start)
                    || has_any_atomic_hook(&hooks.session_end)
                    || has_any_atomic_hook(&hooks.before_agent)
                    || has_any_atomic_hook(&hooks.after_agent)
                    || has_any_atomic_hook(&hooks.before_tool)
                    || has_any_atomic_hook(&hooks.after_tool)
            }
            Err(_) => false,
        }
    }

    // Global install/uninstall (same pattern as Claude Code)

    /// Install hooks globally to `~/.gemini/settings.json`.
    pub fn install_global(&self, force: bool) -> AgentResult<usize> {
        let settings_path =
            Self::global_settings_path().ok_or_else(|| AgentError::ConfigError {
                operation: "resolve home".to_string(),
                path: PathBuf::from("~/.gemini/settings.json"),
                reason: "Could not determine home directory".to_string(),
            })?;

        Self::install_to(&settings_path, force)
    }

    /// Remove hooks from the global `~/.gemini/settings.json`.
    pub fn uninstall_global(&self) -> AgentResult<()> {
        let settings_path = match Self::global_settings_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        Self::uninstall_from(&settings_path)
    }

    /// Check if hooks are installed globally in `~/.gemini/settings.json`.
    pub fn is_installed_global(&self) -> bool {
        match Self::global_settings_path() {
            Some(p) => Self::is_installed_in(&p),
            None => false,
        }
    }
}

// AgentHook Implementation

impl AgentHook for GeminiCliHook {
    fn name(&self) -> &str {
        "gemini-cli"
    }

    fn display_name(&self) -> &str {
        "Gemini CLI"
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

                let session_id = parsed
                    .session_id
                    .or_else(|| {
                        raw_json
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
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

                let session_id = parsed.session_id.unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }

                Ok(event)
            }

            HookType::TurnStart => {
                // Gemini CLI: BeforeAgent → TurnStart
                let parsed: BeforeAgentInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let session_id = parsed.session_id.unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
                }

                Ok(event)
            }

            HookType::TurnEnd => {
                // Gemini CLI: AfterAgent → TurnEnd
                let parsed: AfterAgentInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let session_id = parsed.session_id.unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
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

                let session_id = parsed.session_id.unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(name) = parsed.tool_name {
                    event = event.with_tool_name(name);
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

                let session_id = parsed.session_id.unwrap_or_else(|| "unknown".to_string());

                let mut event = TurnEvent::new(&session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(name) = parsed.tool_name {
                    event = event.with_tool_name(name);
                }

                Ok(event)
            }
        }
    }

    fn install(&self, repo_root: &Path) -> AgentResult<usize> {
        let settings_path = Self::settings_path(repo_root);
        Self::install_to(&settings_path, false)
    }

    fn uninstall(&self, repo_root: &Path) -> AgentResult<()> {
        let settings_path = Self::settings_path(repo_root);
        Self::uninstall_from(&settings_path)
    }

    fn is_installed(&self, repo_root: &Path) -> bool {
        let settings_path = Self::settings_path(repo_root);
        Self::is_installed_in(&settings_path)
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
        repo_root.join(GEMINI_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "before-agent",
            "after-agent",
            "before-tool",
            "after-tool",
        ]
    }
}

// Hook Verb Mapping

/// Map Gemini CLI hook verbs to Atomic HookTypes.
///
/// These are registered in addition to the standard verbs in
/// [`HookType::from_verb`]. The CLI dispatch layer checks both.
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "session-end" => Some(HookType::SessionEnd),
        "before-agent" => Some(HookType::TurnStart),
        "after-agent" => Some(HookType::TurnEnd),
        "before-tool" => Some(HookType::PreToolUse),
        "after-tool" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// Helper Functions

/// Check if a specific hook command already exists in a matcher list.
fn hook_command_exists(matchers: &[GeminiHookMatcher], matcher_str: &str, command: &str) -> bool {
    matchers
        .iter()
        .any(|m| m.matcher == matcher_str && m.hooks.iter().any(|h| h.command == command))
}

/// Add a hook command to a matcher list, creating the matcher if needed.
fn add_hook_to_matcher(
    matchers: &mut Vec<GeminiHookMatcher>,
    matcher_str: &str,
    command: &str,
    name: Option<&str>,
) {
    // Look for an existing matcher with the same string
    for m in matchers.iter_mut() {
        if m.matcher == matcher_str {
            m.hooks.push(GeminiHookEntry {
                hook_type: "command".to_string(),
                command: command.to_string(),
                name: name.map(String::from),
                timeout: None,
                description: None,
            });
            return;
        }
    }

    // Create a new matcher
    matchers.push(GeminiHookMatcher {
        matcher: matcher_str.to_string(),
        hooks: vec![GeminiHookEntry {
            hook_type: "command".to_string(),
            command: command.to_string(),
            name: name.map(String::from),
            timeout: None,
            description: None,
        }],
        sequential: None,
    });
}

/// Returns `true` if a hook command string is an Atomic hook.
fn is_atomic_hook(command: &str) -> bool {
    command.contains(ATOMIC_HOOK_PREFIX)
}

/// Check if any matcher in a hook list contains an Atomic hook.
fn has_any_atomic_hook(matchers: &[GeminiHookMatcher]) -> bool {
    matchers
        .iter()
        .any(|m| m.hooks.iter().any(|h| is_atomic_hook(&h.command)))
}

/// Remove all Atomic hooks from a matcher list.
///
/// Preserves non-Atomic hooks. Removes empty matchers (those with no
/// remaining hooks after Atomic hooks are removed).
fn remove_atomic_hooks(matchers: &mut Vec<GeminiHookMatcher>) {
    for m in matchers.iter_mut() {
        m.hooks.retain(|h| !is_atomic_hook(&h.command));
    }
    // Remove empty matchers
    matchers.retain(|m| !m.hooks.is_empty());
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> GeminiCliHook {
        GeminiCliHook::new()
    }

    // Identity tests

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "gemini-cli");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Gemini CLI");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let supported = hook.supported_hooks();
        assert!(supported.contains(&HookType::SessionStart));
        assert!(supported.contains(&HookType::SessionEnd));
        assert!(supported.contains(&HookType::TurnStart));
        assert!(supported.contains(&HookType::TurnEnd));
        assert!(supported.contains(&HookType::PreToolUse));
        assert!(supported.contains(&HookType::PostToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 6);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"before-agent"));
        assert!(verbs.contains(&"after-agent"));
        assert!(verbs.contains(&"before-tool"));
        assert!(verbs.contains(&"after-tool"));
    }

    // Verb mapping tests

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("session-end"), Some(HookType::SessionEnd));
        assert_eq!(verb_to_hook_type("before-agent"), Some(HookType::TurnStart));
        assert_eq!(verb_to_hook_type("after-agent"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("before-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("after-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    // Parse event tests

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input =
            br#"{"session_id": "sess-123", "transcript_path": "/tmp/t.json", "source": "startup"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert!(event.transcript_path.is_some());
    }

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input =
            br#"{"session_id": "sess-123", "transcript_path": "/tmp/t.json", "reason": "exit"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    #[test]
    fn test_parse_before_agent_turn_start() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-123", "prompt": "Fix the bug in auth.rs"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug in auth.rs"));
    }

    #[test]
    fn test_parse_after_agent_turn_end() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-123", "prompt": "Fix the bug", "prompt_response": "I fixed it"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnEnd);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
    }

    #[test]
    fn test_parse_before_tool() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-123", "tool_name": "write_file", "tool_input": {"path": "test.txt"}}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("write_file"));
    }

    #[test]
    fn test_parse_after_tool() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-123", "tool_name": "read_file", "tool_response": {"content": "hello"}}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("read_file"));
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
        let result = hook.parse_event(HookType::SessionStart, b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_session_id_defaults() {
        let hook = make_hook();
        let input = br#"{"transcript_path": "/tmp/t.json"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "unknown");
    }

    // Detection tests

    #[test]
    fn test_detect_presence_with_gemini_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gemini")).unwrap();

        let hook = make_hook();
        assert!(hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_gemini_dir() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    // Install / uninstall tests

    #[test]
    fn test_install_creates_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        let count = hook.install(dir.path()).unwrap();

        // Should install 6 hooks
        assert_eq!(count, 6);

        // Settings file should exist
        let settings_path = dir.path().join(".gemini").join("settings.json");
        assert!(settings_path.exists());

        // Verify it contains Atomic hooks
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains(ATOMIC_HOOK_PREFIX));
        assert!(content.contains("session-start"));
        assert!(content.contains("session-end"));
        assert!(content.contains("before-agent"));
        assert!(content.contains("after-agent"));
        assert!(content.contains("before-tool"));
        assert!(content.contains("after-tool"));
    }

    #[test]
    fn test_install_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        let count1 = hook.install(dir.path()).unwrap();
        assert_eq!(count1, 6);

        // Second install should find them already present
        let count2 = hook.install(dir.path()).unwrap();
        assert_eq!(count2, 0);
    }

    #[test]
    fn test_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        assert!(!hook.is_installed(dir.path()));

        hook.install(dir.path()).unwrap();

        assert!(hook.is_installed(dir.path()));
    }

    #[test]
    fn test_uninstall_removes_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        hook.install(dir.path()).unwrap();
        assert!(hook.is_installed(dir.path()));

        hook.uninstall(dir.path()).unwrap();
        assert!(!hook.is_installed(dir.path()));

        // Settings file should still exist
        let settings_path = dir.path().join(".gemini").join("settings.json");
        assert!(settings_path.exists());

        // But no Atomic hooks
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(!content.contains(ATOMIC_HOOK_PREFIX));
    }

    #[test]
    fn test_install_preserves_existing_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let gemini_dir = dir.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();

        // Write settings with an existing non-Atomic hook
        let existing = serde_json::json!({
            "hooks": {
                "AfterAgent": [
                    {
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "my-custom-hook --on-stop", "name": "custom"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            gemini_dir.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let hook = make_hook();
        hook.install(dir.path()).unwrap();

        // Verify existing hook is preserved
        let data = std::fs::read_to_string(gemini_dir.join("settings.json")).unwrap();
        assert!(data.contains("my-custom-hook --on-stop"));
        assert!(data.contains(ATOMIC_HOOK_PREFIX));
    }

    #[test]
    fn test_uninstall_preserves_non_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let gemini_dir = dir.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();

        // Write settings with both Atomic and non-Atomic hooks
        let existing = serde_json::json!({
            "hooks": {
                "AfterAgent": [
                    {
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "my-custom-hook --on-stop", "name": "custom"},
                            {"type": "command", "command": "atomic agent hooks gemini-cli after-agent", "name": "atomic-turn-end"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            gemini_dir.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let hook = make_hook();
        hook.uninstall(dir.path()).unwrap();

        let data = std::fs::read_to_string(gemini_dir.join("settings.json")).unwrap();
        assert!(data.contains("my-custom-hook --on-stop"));
        assert!(!data.contains(ATOMIC_HOOK_PREFIX));
    }

    #[test]
    fn test_uninstall_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        let result = hook.uninstall(dir.path());
        assert!(result.is_ok());
    }

    // Helper function tests

    #[test]
    fn test_is_atomic_hook() {
        // Bare (legacy) format
        assert!(is_atomic_hook(
            "atomic agent hooks gemini-cli session-start"
        ));
        assert!(is_atomic_hook("atomic agent hooks gemini-cli after-agent"));
        // Guarded format
        assert!(is_atomic_hook(
            "test -d .atomic && atomic agent hooks gemini-cli session-start || true"
        ));
        assert!(is_atomic_hook(
            "test -d .atomic && atomic agent hooks gemini-cli after-agent || true"
        ));
        // Non-atomic
        assert!(!is_atomic_hook("my-custom-hook --on-stop"));
        assert!(!is_atomic_hook("atomic agent hooks claude-code stop"));
        assert!(!is_atomic_hook(""));
    }

    #[test]
    fn test_hook_command_exists() {
        let matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![GeminiHookEntry {
                hook_type: "command".to_string(),
                command: "atomic agent hooks gemini-cli session-start".to_string(),
                name: Some("atomic-session-start".to_string()),
                timeout: None,
                description: None,
            }],
            sequential: None,
        }];

        assert!(hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks gemini-cli session-start"
        ));
        assert!(!hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks gemini-cli session-end"
        ));
        assert!(!hook_command_exists(
            &matchers,
            "some-matcher",
            "atomic agent hooks gemini-cli session-start"
        ));
    }

    #[test]
    fn test_add_hook_to_matcher_new() {
        let mut matchers = Vec::new();
        add_hook_to_matcher(
            &mut matchers,
            "",
            "atomic agent hooks gemini-cli session-start",
            Some("atomic-session-start"),
        );

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(
            matchers[0].hooks[0].command,
            "atomic agent hooks gemini-cli session-start"
        );
        assert_eq!(
            matchers[0].hooks[0].name.as_deref(),
            Some("atomic-session-start")
        );
    }

    #[test]
    fn test_add_hook_to_matcher_existing() {
        let mut matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![GeminiHookEntry {
                hook_type: "command".to_string(),
                command: "existing-hook".to_string(),
                name: None,
                timeout: None,
                description: None,
            }],
            sequential: None,
        }];

        add_hook_to_matcher(
            &mut matchers,
            "",
            "atomic agent hooks gemini-cli session-start",
            Some("atomic-session-start"),
        );

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 2);
    }

    #[test]
    fn test_remove_atomic_hooks() {
        let mut matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![
                GeminiHookEntry {
                    hook_type: "command".to_string(),
                    command: "my-custom-hook".to_string(),
                    name: None,
                    timeout: None,
                    description: None,
                },
                GeminiHookEntry {
                    hook_type: "command".to_string(),
                    command: "atomic agent hooks gemini-cli after-agent".to_string(),
                    name: Some("atomic-turn-end".to_string()),
                    timeout: None,
                    description: None,
                },
            ],
            sequential: None,
        }];

        remove_atomic_hooks(&mut matchers);

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(matchers[0].hooks[0].command, "my-custom-hook");
    }

    #[test]
    fn test_remove_atomic_hooks_removes_empty_matchers() {
        let mut matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![GeminiHookEntry {
                hook_type: "command".to_string(),
                command: "atomic agent hooks gemini-cli session-start".to_string(),
                name: None,
                timeout: None,
                description: None,
            }],
            sequential: None,
        }];

        remove_atomic_hooks(&mut matchers);

        assert!(matchers.is_empty());
    }

    #[test]
    fn test_has_any_atomic_hook_true() {
        let matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![GeminiHookEntry {
                hook_type: "command".to_string(),
                command: "atomic agent hooks gemini-cli session-start".to_string(),
                name: None,
                timeout: None,
                description: None,
            }],
            sequential: None,
        }];

        assert!(has_any_atomic_hook(&matchers));
    }

    #[test]
    fn test_has_any_atomic_hook_false() {
        let matchers = vec![GeminiHookMatcher {
            matcher: "".to_string(),
            hooks: vec![GeminiHookEntry {
                hook_type: "command".to_string(),
                command: "other-hook".to_string(),
                name: None,
                timeout: None,
                description: None,
            }],
            sequential: None,
        }];

        assert!(!has_any_atomic_hook(&matchers));
    }

    #[test]
    fn test_has_any_atomic_hook_empty() {
        let matchers: Vec<GeminiHookMatcher> = Vec::new();
        assert!(!has_any_atomic_hook(&matchers));
    }

    // Settings serialization roundtrip

    #[test]
    fn test_settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        // Install hooks
        hook.install(dir.path()).unwrap();

        // Read back and verify structure
        let settings_path = dir.path().join(".gemini").join("settings.json");
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Check that hooks are under PascalCase keys
        let hooks = parsed.get("hooks").unwrap();
        assert!(hooks.get("SessionStart").is_some());
        assert!(hooks.get("SessionEnd").is_some());
        assert!(hooks.get("BeforeAgent").is_some());
        assert!(hooks.get("AfterAgent").is_some());
        assert!(hooks.get("BeforeTool").is_some());
        assert!(hooks.get("AfterTool").is_some());
    }

    #[test]
    fn test_settings_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let gemini_dir = dir.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();

        // Write settings with extra fields
        let existing = serde_json::json!({
            "hooks": {},
            "customField": "should be preserved",
            "nested": {"key": "value"}
        });
        std::fs::write(
            gemini_dir.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let hook = make_hook();
        hook.install(dir.path()).unwrap();

        let data = std::fs::read_to_string(gemini_dir.join("settings.json")).unwrap();
        assert!(data.contains("customField"));
        assert!(data.contains("should be preserved"));
        assert!(data.contains("nested"));
    }
}

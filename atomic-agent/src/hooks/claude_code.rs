//! Claude Code hook adapter for Atomic Agent.
//!
//! This module implements the [`AgentHook`] trait for Claude Code, handling:
//!
//! - **JSON parsing** of hook callbacks from stdin
//! - **Hook installation** into `.claude/settings.json`
//! - **Hook removal** preserving non-Atomic hooks
//! - **Presence detection** via `.claude/` directory
//!
//! # Claude Code Hook Architecture
//!
//! Claude Code fires hooks at specific lifecycle points. Each hook receives
//! JSON on stdin and can return JSON on stdout to inject system messages.
//!
//! | Claude Code Hook      | Atomic HookType    | JSON Fields                              |
//! |-----------------------|--------------------|------------------------------------------|
//! | `session-start`       | SessionStart       | `session_id`, `transcript_path`          |
//! | `session-end`         | SessionEnd         | `session_id`, `transcript_path`          |
//! | `user-prompt-submit`  | TurnStart          | `session_id`, `transcript_path`, `prompt`|
//! | `stop`                | TurnEnd            | `session_id`, `transcript_path`          |
//! | `pre-task`            | PreToolUse         | `session_id`, `transcript_path`, `tool_use_id`, `tool_input` |
//! | `post-task`           | PostToolUse        | `session_id`, `transcript_path`, `tool_use_id`, `tool_input`, `tool_response` |
//! | `post-todo`           | PostToolUse        | `session_id`, `transcript_path`, `tool_use_id`, `tool_input`, `tool_response` |
//!
//! # Settings File Format
//!
//! Claude Code reads hooks from `.claude/settings.json`:
//!
//! ```json
//! {
//!   "hooks": {
//!     "stop": [
//!       {
//!         "matcher": "",
//!         "hooks": [
//!           { "type": "command", "command": "atomic agent hooks claude-code stop" }
//!         ]
//!       }
//!     ],
//!     "PreToolUse": [
//!       {
//!         "matcher": "Task",
//!         "hooks": [
//!           { "type": "command", "command": "atomic agent hooks claude-code pre-task" }
//!         ]
//!       }
//!     ]
//!   },
//!   "permissions": {
//!     "deny": ["Read(./.atomic/metadata/**)"]
//!   }
//! }
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::hooks::claude_code::ClaudeCodeHook;
//! use atomic_agent::hooks::AgentHook;
//! use atomic_agent::event::HookType;
//!
//! let hook = ClaudeCodeHook::new();
//! assert_eq!(hook.name(), "claude-code");
//! assert_eq!(hook.display_name(), "Claude Code");
//!
//! // Parse a stop hook input
//! let input = br#"{"session_id": "abc-123", "transcript_path": "/tmp/t.jsonl"}"#;
//! let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
//! assert_eq!(event.session_id, "abc-123");
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

use super::AgentHook;

// Constants

/// The directory where Claude Code stores per-project settings.
const CLAUDE_DIR: &str = ".claude";

/// The settings file that Claude Code reads hooks from.
const SETTINGS_FILE: &str = "settings.json";

/// Command prefix used to identify Atomic hooks in the settings file.
const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks claude-code";

/// Permission deny rule to prevent Claude from reading Atomic metadata.
const METADATA_DENY_RULE: &str = "Read(./.atomic/metadata/**)";

// Claude Code JSON Input Types

/// JSON input for session-end and stop hooks.
#[derive(Debug, Deserialize)]
struct SessionInfoInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

/// JSON input for session-start hook.
///
/// Unlike other session hooks, SessionStart includes `model` (the Claude model
/// identifier, e.g. "claude-sonnet-4-5-20250929") and `source` (how the session
/// started: "startup", "resume", "clear", "compact").
///
/// See: https://code.claude.com/docs/en/hooks#sessionstart-input
#[derive(Debug, Deserialize)]
struct SessionStartInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    /// The model identifier (e.g., "claude-sonnet-4-5-20250929").
    #[serde(default)]
    model: Option<String>,
    /// How the session started: "startup", "resume", "clear", "compact".
    #[serde(default)]
    source: Option<String>,
}

/// JSON input for user-prompt-submit hook.
#[derive(Debug, Deserialize)]
struct UserPromptInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

/// JSON input for pre-task (PreToolUse[Task]) hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreToolInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

/// JSON input for post-task and post-todo (PostToolUse) hooks.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_response: Option<serde_json::Value>,
}

// Claude Code Settings File Types

/// A single hook entry within a matcher group.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaudeHookEntry {
    #[serde(rename = "type")]
    hook_type: String,
    command: String,
}

/// A matcher group containing one or more hook entries.
///
/// For simple hooks (stop, session-start, etc.) the `matcher` is empty.
/// For tool-specific hooks (PreToolUse, PostToolUse) the `matcher` is the
/// tool name (e.g., "Task", "TodoWrite").
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaudeHookMatcher {
    #[serde(default)]
    matcher: String,
    hooks: Vec<ClaudeHookEntry>,
}

/// The hooks section of `.claude/settings.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClaudeHooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_start: Vec<ClaudeHookMatcher>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_end: Vec<ClaudeHookMatcher>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stop: Vec<ClaudeHookMatcher>,

    #[serde(
        default,
        rename = "UserPromptSubmit",
        skip_serializing_if = "Vec::is_empty"
    )]
    user_prompt_submit: Vec<ClaudeHookMatcher>,

    #[serde(default, rename = "PreToolUse", skip_serializing_if = "Vec::is_empty")]
    pre_tool_use: Vec<ClaudeHookMatcher>,

    #[serde(default, rename = "PostToolUse", skip_serializing_if = "Vec::is_empty")]
    post_tool_use: Vec<ClaudeHookMatcher>,
}

// ClaudeCodeHook

/// Claude Code agent hook adapter.
///
/// Handles hook JSON parsing, installation into `.claude/settings.json`,
/// and presence detection via the `.claude/` directory.
#[derive(Debug)]
pub struct ClaudeCodeHook {
    _private: (), // prevent construction outside of new()
}

impl ClaudeCodeHook {
    /// Create a new Claude Code hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Returns the path to `.claude/settings.json` relative to the repo root.
    fn settings_path(repo_root: &Path) -> PathBuf {
        repo_root.join(CLAUDE_DIR).join(SETTINGS_FILE)
    }

    /// Returns the path to the global `~/.claude/settings.json`.
    ///
    /// Global hooks apply to every Claude Code session regardless of project.
    /// This is the recommended way to enable Atomic tracking — install once,
    /// works everywhere that has a `.atomic/` directory.
    pub fn global_settings_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(CLAUDE_DIR).join(SETTINGS_FILE))
    }

    /// Install hooks globally to `~/.claude/settings.json`.
    ///
    /// Unlike `install()` which writes to the project's `.claude/settings.json`,
    /// this writes to the user's home directory so hooks fire for every project.
    ///
    /// # Returns
    ///
    /// The number of hooks installed (0 if already up to date).
    pub fn install_global(&self, force: bool) -> AgentResult<usize> {
        let settings_path =
            Self::global_settings_path().ok_or_else(|| AgentError::ConfigError {
                operation: "resolve home".to_string(),
                path: PathBuf::from("~/.claude/settings.json"),
                reason: "Could not determine home directory".to_string(),
            })?;

        // Create ~/.claude/ if needed
        if let Some(parent) = settings_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
                    operation: "create directory".to_string(),
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
            }
        }

        let (mut raw, mut hooks) = Self::read_settings(&settings_path)?;

        // If forcing, remove existing atomic hooks first
        if force {
            remove_atomic_hooks(&mut hooks.session_start);
            remove_atomic_hooks(&mut hooks.session_end);
            remove_atomic_hooks(&mut hooks.stop);
            remove_atomic_hooks(&mut hooks.user_prompt_submit);
            remove_atomic_hooks(&mut hooks.pre_tool_use);
            remove_atomic_hooks(&mut hooks.post_tool_use);
        }

        // Install hooks using the same definitions as the project-level install
        let hook_defs: Vec<(&str, &str, &str)> = vec![
            ("session-start", "", "session-start"),
            ("session-end", "", "session-end"),
            ("stop", "", "stop"),
            ("user-prompt-submit", "", "user-prompt-submit"),
            ("pre-task", "Task", "pre-task"),
            ("post-task", "Task", "post-task"),
            ("post-todo", "TodoWrite", "post-todo"),
        ];

        let mut count = 0;
        for (_label, matcher, verb) in &hook_defs {
            let command = format!("{} {}", ATOMIC_HOOK_PREFIX, verb);

            let matchers = match *verb {
                "session-start" => &mut hooks.session_start,
                "session-end" => &mut hooks.session_end,
                "stop" => &mut hooks.stop,
                "user-prompt-submit" => &mut hooks.user_prompt_submit,
                "pre-task" => &mut hooks.pre_tool_use,
                "post-task" | "post-todo" => &mut hooks.post_tool_use,
                _ => continue,
            };

            if !hook_command_exists(matchers, matcher, &command) {
                add_hook_to_matcher(matchers, matcher, &command);
                count += 1;
            }
        }

        // Add permissions.deny rule
        let permissions_changed = ensure_deny_rule(&mut raw);

        if count == 0 && !permissions_changed {
            return Ok(0);
        }

        // Serialize hooks back into raw settings
        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(&settings_path, &raw)?;

        Ok(count)
    }

    /// Remove hooks from the global `~/.claude/settings.json`.
    pub fn uninstall_global(&self) -> AgentResult<()> {
        let settings_path = match Self::global_settings_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        if !settings_path.exists() {
            return Ok(());
        }

        let (mut raw, mut hooks) = Self::read_settings(&settings_path)?;

        remove_atomic_hooks(&mut hooks.session_start);
        remove_atomic_hooks(&mut hooks.session_end);
        remove_atomic_hooks(&mut hooks.stop);
        remove_atomic_hooks(&mut hooks.user_prompt_submit);
        remove_atomic_hooks(&mut hooks.pre_tool_use);
        remove_atomic_hooks(&mut hooks.post_tool_use);

        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(&settings_path, &raw)?;

        Ok(())
    }

    /// Check if hooks are installed globally in `~/.claude/settings.json`.
    pub fn is_installed_global(&self) -> bool {
        let settings_path = match Self::global_settings_path() {
            Some(p) => p,
            None => return false,
        };

        if !settings_path.exists() {
            return false;
        }

        match Self::read_settings(&settings_path) {
            Ok((_, hooks)) => {
                // Check if any hook list contains an atomic hook
                has_any_atomic_hook(&hooks.session_start)
                    || has_any_atomic_hook(&hooks.session_end)
                    || has_any_atomic_hook(&hooks.stop)
                    || has_any_atomic_hook(&hooks.user_prompt_submit)
                    || has_any_atomic_hook(&hooks.pre_tool_use)
                    || has_any_atomic_hook(&hooks.post_tool_use)
            }
            Err(_) => false,
        }
    }

    /// Read and parse the existing `.claude/settings.json`, if it exists.
    ///
    /// Returns `(raw_settings, hooks)` where `raw_settings` preserves unknown
    /// fields and `hooks` is the parsed hooks section.
    fn read_settings(
        settings_path: &Path,
    ) -> AgentResult<(serde_json::Map<String, serde_json::Value>, ClaudeHooks)> {
        let data = match std::fs::read(settings_path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((serde_json::Map::new(), ClaudeHooks::default()));
            }
            Err(e) => {
                return Err(AgentError::ConfigError {
                    operation: "read".to_string(),
                    path: settings_path.to_path_buf(),
                    reason: e.to_string(),
                });
            }
        };

        let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&data)
            .map_err(|e| AgentError::ConfigError {
                operation: "parse".to_string(),
                path: settings_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        let hooks = if let Some(hooks_val) = raw.get("hooks") {
            serde_json::from_value(hooks_val.clone()).map_err(|e| AgentError::ConfigError {
                operation: "parse hooks".to_string(),
                path: settings_path.to_path_buf(),
                reason: e.to_string(),
            })?
        } else {
            ClaudeHooks::default()
        };

        Ok((raw, hooks))
    }

    /// Write settings back to `.claude/settings.json`, preserving formatting.
    fn write_settings(
        settings_path: &Path,
        raw: &serde_json::Map<String, serde_json::Value>,
    ) -> AgentResult<()> {
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
                operation: "create directory".to_string(),
                path: parent.to_path_buf(),
                reason: e.to_string(),
            })?;
        }

        let mut output =
            serde_json::to_string_pretty(raw).map_err(|e| AgentError::ConfigError {
                operation: "serialize".to_string(),
                path: settings_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        // Ensure trailing newline for POSIX compatibility
        if !output.ends_with('\n') {
            output.push('\n');
        }

        std::fs::write(settings_path, output.as_bytes()).map_err(|e| AgentError::ConfigError {
            operation: "write".to_string(),
            path: settings_path.to_path_buf(),
            reason: e.to_string(),
        })?;

        Ok(())
    }

    /// Extract the session_id from parsed input, returning a default if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl Default for ClaudeCodeHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for ClaudeCodeHook {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn display_name(&self) -> &str {
        "Claude Code"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        // Preserve raw JSON for debugging
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

            HookType::SessionEnd | HookType::TurnEnd => {
                let parsed: SessionInfoInput =
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

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }

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

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
                }

                Ok(event)
            }

            HookType::PreToolUse => {
                let parsed: PreToolInput =
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

            HookType::PostToolUse => {
                let parsed: PostToolInput =
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
        }
    }

    fn install(&self, repo_root: &Path) -> AgentResult<usize> {
        let settings_path = Self::settings_path(repo_root);
        let (mut raw, mut hooks) = Self::read_settings(&settings_path)?;

        let mut count = 0;

        // Define the hooks to install: (settings key field, matcher, verb)
        let hook_defs: Vec<(&str, &str, &str)> = vec![
            ("session-start", "", "session-start"),
            ("session-end", "", "session-end"),
            ("stop", "", "stop"),
            ("user-prompt-submit", "", "user-prompt-submit"),
            ("pre-task", "Task", "pre-task"),
            ("post-task", "Task", "post-task"),
            ("post-todo", "TodoWrite", "post-todo"),
        ];

        for (_label, matcher, verb) in &hook_defs {
            let command = format!("{} {}", ATOMIC_HOOK_PREFIX, verb);

            let matchers = match *verb {
                "session-start" => &mut hooks.session_start,
                "session-end" => &mut hooks.session_end,
                "stop" => &mut hooks.stop,
                "user-prompt-submit" => &mut hooks.user_prompt_submit,
                "pre-task" => &mut hooks.pre_tool_use,
                "post-task" | "post-todo" => &mut hooks.post_tool_use,
                _ => continue,
            };

            if !hook_command_exists(matchers, matcher, &command) {
                add_hook_to_matcher(matchers, matcher, &command);
                count += 1;
            }
        }

        // Add permissions.deny rule to prevent Claude from reading metadata
        let permissions_changed = ensure_deny_rule(&mut raw);

        if count == 0 && !permissions_changed {
            return Ok(0);
        }

        // Serialize hooks back into raw settings
        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(&settings_path, &raw)?;

        Ok(count)
    }

    fn uninstall(&self, repo_root: &Path) -> AgentResult<()> {
        let settings_path = Self::settings_path(repo_root);

        if !settings_path.exists() {
            return Ok(()); // Nothing to uninstall
        }

        let (mut raw, mut hooks) = Self::read_settings(&settings_path)?;

        // Remove all Atomic hooks from every hook list
        remove_atomic_hooks(&mut hooks.session_start);
        remove_atomic_hooks(&mut hooks.session_end);
        remove_atomic_hooks(&mut hooks.stop);
        remove_atomic_hooks(&mut hooks.user_prompt_submit);
        remove_atomic_hooks(&mut hooks.pre_tool_use);
        remove_atomic_hooks(&mut hooks.post_tool_use);

        // Remove the metadata deny rule from permissions
        remove_deny_rule(&mut raw);

        // Serialize hooks back
        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);

        Self::write_settings(&settings_path, &raw)?;

        Ok(())
    }

    fn is_installed(&self, repo_root: &Path) -> bool {
        let settings_path = Self::settings_path(repo_root);

        let Ok(data) = std::fs::read(&settings_path) else {
            return false;
        };

        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return false;
        };

        // Check if any hook command starts with our prefix
        let json_str = raw.to_string();
        json_str.contains(ATOMIC_HOOK_PREFIX)
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
        repo_root.join(CLAUDE_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "stop",
            "user-prompt-submit",
            "pre-task",
            "post-task",
            "post-todo",
        ]
    }
}

// Hook Manipulation Helpers

/// Check if a specific command exists in a matcher list.
fn hook_command_exists(matchers: &[ClaudeHookMatcher], matcher_name: &str, command: &str) -> bool {
    for matcher in matchers {
        if matcher.matcher == matcher_name {
            for hook in &matcher.hooks {
                if hook.command == command {
                    return true;
                }
            }
        }
    }
    false
}

/// Add a hook command to the appropriate matcher in the list.
///
/// If a matcher with the given name already exists, the hook is appended to it.
/// Otherwise, a new matcher group is created.
fn add_hook_to_matcher(matchers: &mut Vec<ClaudeHookMatcher>, matcher_name: &str, command: &str) {
    let entry = ClaudeHookEntry {
        hook_type: "command".to_string(),
        command: command.to_string(),
    };

    // Find existing matcher with the same name
    for matcher in matchers.iter_mut() {
        if matcher.matcher == matcher_name {
            matcher.hooks.push(entry);
            return;
        }
    }

    // No existing matcher — create a new one
    matchers.push(ClaudeHookMatcher {
        matcher: matcher_name.to_string(),
        hooks: vec![entry],
    });
}

/// Returns `true` if a hook command string is an Atomic hook.
/// Check if any matcher in a hook list contains an Atomic hook.
fn has_any_atomic_hook(matchers: &[ClaudeHookMatcher]) -> bool {
    matchers.iter().any(|m| {
        m.hooks
            .iter()
            .any(|h| h.command.starts_with(ATOMIC_HOOK_PREFIX))
    })
}

fn is_atomic_hook(command: &str) -> bool {
    command.starts_with(ATOMIC_HOOK_PREFIX)
}

/// Remove all Atomic hooks from a matcher list.
///
/// Preserves non-Atomic hooks. Removes empty matchers after filtering.
fn remove_atomic_hooks(matchers: &mut Vec<ClaudeHookMatcher>) {
    for matcher in matchers.iter_mut() {
        matcher.hooks.retain(|h| !is_atomic_hook(&h.command));
    }
    // Remove empty matchers
    matchers.retain(|m| !m.hooks.is_empty());
}

/// Ensure the metadata deny rule exists in permissions.deny.
///
/// Returns `true` if the rule was added (i.e., it wasn't already present).
fn ensure_deny_rule(raw: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let permissions = raw
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::json!({}));

    let permissions_obj = match permissions.as_object_mut() {
        Some(obj) => obj,
        None => return false,
    };

    let deny = permissions_obj
        .entry("deny".to_string())
        .or_insert_with(|| serde_json::json!([]));

    let deny_arr = match deny.as_array_mut() {
        Some(arr) => arr,
        None => return false,
    };

    // Check if already present
    let rule_value = serde_json::Value::String(METADATA_DENY_RULE.to_string());
    if deny_arr.contains(&rule_value) {
        return false;
    }

    deny_arr.push(rule_value);
    true
}

/// Remove the metadata deny rule from permissions.deny.
fn remove_deny_rule(raw: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(permissions) = raw.get_mut("permissions") else {
        return;
    };
    let Some(permissions_obj) = permissions.as_object_mut() else {
        return;
    };
    let Some(deny) = permissions_obj.get_mut("deny") else {
        return;
    };
    let Some(deny_arr) = deny.as_array_mut() else {
        return;
    };

    let rule_value = serde_json::Value::String(METADATA_DENY_RULE.to_string());
    deny_arr.retain(|v| v != &rule_value);

    // Clean up empty deny array
    if deny_arr.is_empty() {
        permissions_obj.remove("deny");
    }

    // Clean up empty permissions object
    if permissions_obj.is_empty() {
        raw.remove("permissions");
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> ClaudeCodeHook {
        ClaudeCodeHook::new()
    }

    // Trait basics

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "claude-code");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Claude Code");
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
        assert_eq!(verbs.len(), 7);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"user-prompt-submit"));
        assert!(verbs.contains(&"pre-task"));
        assert!(verbs.contains(&"post-task"));
        assert!(verbs.contains(&"post-todo"));
    }

    #[test]
    fn test_default() {
        let hook = ClaudeCodeHook::default();
        assert_eq!(hook.name(), "claude-code");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("ClaudeCodeHook"));
    }

    // parse_event — empty input

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnEnd, b"");
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::HookInputEmpty { agent, hook_type } => {
                assert_eq!(agent, "claude-code");
                assert_eq!(hook_type, "turn_end");
            }
            other => panic!("Expected HookInputEmpty, got: {:?}", other),
        }
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnEnd, b"not json");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            AgentError::HookParseFailed { .. }
        ));
    }

    // parse_event — SessionStart / SessionEnd / TurnEnd (SessionInfoInput)

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-1", "transcript_path": "/tmp/t.jsonl"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();

        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert_eq!(event.transcript_path, Some(PathBuf::from("/tmp/t.jsonl")));
        assert!(event.prompt.is_none());
        assert!(event.raw_json.is_some());
    }

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-2"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();

        assert_eq!(event.session_id, "sess-2");
        assert_eq!(event.event_type, HookType::SessionEnd);
        assert!(event.transcript_path.is_none());
    }

    #[test]
    fn test_parse_turn_end_stop() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-3", "transcript_path": "/t.jsonl"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();

        assert_eq!(event.session_id, "sess-3");
        assert_eq!(event.event_type, HookType::TurnEnd);
        assert_eq!(event.transcript_path, Some(PathBuf::from("/t.jsonl")));
    }

    #[test]
    fn test_parse_session_info_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"transcript_path": "/tmp/t.jsonl"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "unknown");
    }

    #[test]
    fn test_parse_session_info_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": "", "transcript_path": "/tmp/t.jsonl"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "unknown");
    }

    #[test]
    fn test_parse_session_info_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "transcript_path": "/t", "extra_field": "ignored"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    // parse_event — TurnStart (UserPromptInput)

    #[test]
    fn test_parse_turn_start_with_prompt() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-4",
            "transcript_path": "/tmp/t.jsonl",
            "prompt": "Fix the authentication bug"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();

        assert_eq!(event.session_id, "sess-4");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.transcript_path, Some(PathBuf::from("/tmp/t.jsonl")));
        assert_eq!(event.prompt, Some("Fix the authentication bug".to_string()));
    }

    #[test]
    fn test_parse_turn_start_no_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-5"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();

        assert_eq!(event.session_id, "sess-5");
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_turn_start_empty_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-6", "prompt": ""}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();

        // Empty string is preserved (not converted to None)
        assert_eq!(event.prompt, Some("".to_string()));
    }

    // parse_event — PreToolUse

    #[test]
    fn test_parse_pre_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-7",
            "transcript_path": "/t.jsonl",
            "tool_use_id": "tu-001",
            "tool_input": {"description": "run tests"}
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();

        assert_eq!(event.session_id, "sess-7");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_use_id, Some("tu-001".to_string()));
        // tool_name is not set from JSON input for PreToolUse (comes from matcher context)
        assert!(event.tool_name.is_none());
    }

    #[test]
    fn test_parse_pre_tool_use_minimal() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-8"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();

        assert_eq!(event.session_id, "sess-8");
        assert!(event.tool_use_id.is_none());
    }

    // parse_event — PostToolUse

    #[test]
    fn test_parse_post_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-9",
            "transcript_path": "/t.jsonl",
            "tool_use_id": "tu-002",
            "tool_name": "Task",
            "tool_input": {"description": "implement feature"},
            "tool_response": {"agentId": "agent-abc"}
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();

        assert_eq!(event.session_id, "sess-9");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_use_id, Some("tu-002".to_string()));
        assert_eq!(event.tool_name, Some("Task".to_string()));
    }

    #[test]
    fn test_parse_post_tool_use_todo_write() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-10",
            "tool_use_id": "tu-003",
            "tool_name": "TodoWrite",
            "tool_input": {"todos": []}
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();

        assert_eq!(event.tool_name, Some("TodoWrite".to_string()));
    }

    // Hook manipulation helpers

    #[test]
    fn test_is_atomic_hook() {
        assert!(is_atomic_hook("atomic agent hooks claude-code stop"));
        assert!(is_atomic_hook(
            "atomic agent hooks claude-code user-prompt-submit"
        ));
        assert!(!is_atomic_hook("entire hooks claude-code stop"));
        assert!(!is_atomic_hook("some other command"));
        assert!(!is_atomic_hook(""));
    }

    #[test]
    fn test_hook_command_exists_found() {
        let matchers = vec![ClaudeHookMatcher {
            matcher: "".to_string(),
            hooks: vec![ClaudeHookEntry {
                hook_type: "command".to_string(),
                command: "atomic agent hooks claude-code stop".to_string(),
            }],
        }];
        assert!(hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks claude-code stop"
        ));
    }

    #[test]
    fn test_hook_command_exists_not_found() {
        let matchers = vec![ClaudeHookMatcher {
            matcher: "".to_string(),
            hooks: vec![ClaudeHookEntry {
                hook_type: "command".to_string(),
                command: "some other hook".to_string(),
            }],
        }];
        assert!(!hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks claude-code stop"
        ));
    }

    #[test]
    fn test_hook_command_exists_wrong_matcher() {
        let matchers = vec![ClaudeHookMatcher {
            matcher: "Task".to_string(),
            hooks: vec![ClaudeHookEntry {
                hook_type: "command".to_string(),
                command: "atomic agent hooks claude-code pre-task".to_string(),
            }],
        }];
        // Looking for empty matcher, but hook is under "Task"
        assert!(!hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks claude-code pre-task"
        ));
        // Looking for correct matcher
        assert!(hook_command_exists(
            &matchers,
            "Task",
            "atomic agent hooks claude-code pre-task"
        ));
    }

    #[test]
    fn test_hook_command_exists_empty_list() {
        let matchers: Vec<ClaudeHookMatcher> = vec![];
        assert!(!hook_command_exists(
            &matchers,
            "",
            "atomic agent hooks claude-code stop"
        ));
    }

    #[test]
    fn test_add_hook_to_matcher_new_matcher() {
        let mut matchers: Vec<ClaudeHookMatcher> = vec![];
        add_hook_to_matcher(&mut matchers, "", "atomic agent hooks claude-code stop");

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].matcher, "");
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(
            matchers[0].hooks[0].command,
            "atomic agent hooks claude-code stop"
        );
        assert_eq!(matchers[0].hooks[0].hook_type, "command");
    }

    #[test]
    fn test_add_hook_to_matcher_existing_matcher() {
        let mut matchers = vec![ClaudeHookMatcher {
            matcher: "".to_string(),
            hooks: vec![ClaudeHookEntry {
                hook_type: "command".to_string(),
                command: "existing hook".to_string(),
            }],
        }];

        add_hook_to_matcher(&mut matchers, "", "atomic agent hooks claude-code stop");

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 2);
        assert_eq!(matchers[0].hooks[0].command, "existing hook");
        assert_eq!(
            matchers[0].hooks[1].command,
            "atomic agent hooks claude-code stop"
        );
    }

    #[test]
    fn test_add_hook_to_matcher_named_matcher() {
        let mut matchers: Vec<ClaudeHookMatcher> = vec![];
        add_hook_to_matcher(
            &mut matchers,
            "Task",
            "atomic agent hooks claude-code pre-task",
        );

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].matcher, "Task");
        assert_eq!(matchers[0].hooks.len(), 1);
    }

    #[test]
    fn test_remove_atomic_hooks_preserves_others() {
        let mut matchers = vec![ClaudeHookMatcher {
            matcher: "".to_string(),
            hooks: vec![
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "some other hook".to_string(),
                },
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "atomic agent hooks claude-code stop".to_string(),
                },
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "another non-atomic hook".to_string(),
                },
            ],
        }];

        remove_atomic_hooks(&mut matchers);

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 2);
        assert_eq!(matchers[0].hooks[0].command, "some other hook");
        assert_eq!(matchers[0].hooks[1].command, "another non-atomic hook");
    }

    #[test]
    fn test_remove_atomic_hooks_removes_empty_matchers() {
        let mut matchers = vec![
            ClaudeHookMatcher {
                matcher: "".to_string(),
                hooks: vec![ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "atomic agent hooks claude-code stop".to_string(),
                }],
            },
            ClaudeHookMatcher {
                matcher: "".to_string(),
                hooks: vec![ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "keep this".to_string(),
                }],
            },
        ];

        remove_atomic_hooks(&mut matchers);

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks[0].command, "keep this");
    }

    #[test]
    fn test_remove_atomic_hooks_all_removed() {
        let mut matchers = vec![ClaudeHookMatcher {
            matcher: "Task".to_string(),
            hooks: vec![
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "atomic agent hooks claude-code pre-task".to_string(),
                },
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "atomic agent hooks claude-code post-task".to_string(),
                },
            ],
        }];

        remove_atomic_hooks(&mut matchers);

        assert!(matchers.is_empty());
    }

    // Deny rule helpers

    #[test]
    fn test_ensure_deny_rule_adds_when_missing() {
        let mut raw = serde_json::Map::new();
        let changed = ensure_deny_rule(&mut raw);

        assert!(changed);
        let deny = raw["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 1);
        assert_eq!(deny[0].as_str().unwrap(), METADATA_DENY_RULE);
    }

    #[test]
    fn test_ensure_deny_rule_no_duplicate() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "permissions".to_string(),
            serde_json::json!({
                "deny": [METADATA_DENY_RULE]
            }),
        );

        let changed = ensure_deny_rule(&mut raw);
        assert!(!changed);

        let deny = raw["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 1);
    }

    #[test]
    fn test_ensure_deny_rule_preserves_existing_rules() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "permissions".to_string(),
            serde_json::json!({
                "deny": ["Read(some_other_rule)"]
            }),
        );

        let changed = ensure_deny_rule(&mut raw);
        assert!(changed);

        let deny = raw["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 2);
        assert_eq!(deny[0].as_str().unwrap(), "Read(some_other_rule)");
        assert_eq!(deny[1].as_str().unwrap(), METADATA_DENY_RULE);
    }

    #[test]
    fn test_remove_deny_rule() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "permissions".to_string(),
            serde_json::json!({
                "deny": [METADATA_DENY_RULE, "other_rule"]
            }),
        );

        remove_deny_rule(&mut raw);

        let deny = raw["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 1);
        assert_eq!(deny[0].as_str().unwrap(), "other_rule");
    }

    #[test]
    fn test_remove_deny_rule_cleans_up_empty_deny() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "permissions".to_string(),
            serde_json::json!({
                "deny": [METADATA_DENY_RULE]
            }),
        );

        remove_deny_rule(&mut raw);

        // Empty deny array should be removed
        assert!(raw.get("permissions").is_none());
    }

    #[test]
    fn test_remove_deny_rule_no_permissions() {
        let mut raw = serde_json::Map::new();
        // No permissions section at all — should not panic
        remove_deny_rule(&mut raw);
        assert!(raw.get("permissions").is_none());
    }

    #[test]
    fn test_remove_deny_rule_preserves_other_permissions() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "permissions".to_string(),
            serde_json::json!({
                "deny": [METADATA_DENY_RULE],
                "allow": ["Write(some_path)"]
            }),
        );

        remove_deny_rule(&mut raw);

        // permissions should still exist because "allow" is there
        assert!(raw.get("permissions").is_some());
        let perms = raw["permissions"].as_object().unwrap();
        assert!(perms.get("deny").is_none()); // deny removed
        assert!(perms.get("allow").is_some()); // allow preserved
    }

    // Install / Uninstall (filesystem tests)

    #[test]
    fn test_install_creates_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        let count = hook.install(dir.path()).unwrap();

        // Should install 7 hooks
        assert_eq!(count, 7);

        // Settings file should exist
        let settings_path = dir.path().join(".claude").join("settings.json");
        assert!(settings_path.exists());

        // Read and verify
        let data = std::fs::read_to_string(&settings_path).unwrap();
        assert!(data.contains("atomic agent hooks claude-code stop"));
        assert!(data.contains("atomic agent hooks claude-code user-prompt-submit"));
        assert!(data.contains("atomic agent hooks claude-code session-start"));
        assert!(data.contains("atomic agent hooks claude-code session-end"));
        assert!(data.contains("atomic agent hooks claude-code pre-task"));
        assert!(data.contains("atomic agent hooks claude-code post-task"));
        assert!(data.contains("atomic agent hooks claude-code post-todo"));
        assert!(data.contains(METADATA_DENY_RULE));
    }

    #[test]
    fn test_install_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        let count1 = hook.install(dir.path()).unwrap();
        assert_eq!(count1, 7);

        // Second install should return 0 (nothing new to install)
        let count2 = hook.install(dir.path()).unwrap();
        assert_eq!(count2, 0);
    }

    #[test]
    fn test_install_preserves_existing_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        // Write existing settings with a non-Atomic hook
        let existing = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "my-custom-hook --on-stop"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let hook = make_hook();
        hook.install(dir.path()).unwrap();

        // Verify existing hook is preserved
        let data = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert!(data.contains("my-custom-hook --on-stop"));
        assert!(data.contains("atomic agent hooks claude-code stop"));
    }

    #[test]
    fn test_uninstall_removes_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        // Install first
        hook.install(dir.path()).unwrap();

        // Then uninstall
        hook.uninstall(dir.path()).unwrap();

        // Verify hooks are gone
        let settings_path = dir.path().join(".claude").join("settings.json");
        let data = std::fs::read_to_string(&settings_path).unwrap();
        assert!(!data.contains(ATOMIC_HOOK_PREFIX));
        assert!(!data.contains(METADATA_DENY_RULE));
    }

    #[test]
    fn test_uninstall_preserves_non_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        // Write settings with both Atomic and non-Atomic hooks
        let existing = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "my-custom-hook --on-stop"},
                            {"type": "command", "command": "atomic agent hooks claude-code stop"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let hook = make_hook();
        hook.uninstall(dir.path()).unwrap();

        let data = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert!(data.contains("my-custom-hook --on-stop"));
        assert!(!data.contains(ATOMIC_HOOK_PREFIX));
    }

    #[test]
    fn test_uninstall_no_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        // Should not error when there's nothing to uninstall
        assert!(hook.uninstall(dir.path()).is_ok());
    }

    // is_installed

    #[test]
    fn test_is_installed_true() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        hook.install(dir.path()).unwrap();
        assert!(hook.is_installed(dir.path()));
    }

    #[test]
    fn test_is_installed_false_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.is_installed(dir.path()));
    }

    #[test]
    fn test_is_installed_false_no_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"hooks": {"Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "other-tool stop"}]}]}}"#,
        )
        .unwrap();

        let hook = make_hook();
        assert!(!hook.is_installed(dir.path()));
    }

    // detect_presence

    #[test]
    fn test_detect_presence_true() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_false() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_file_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Create .claude as a file, not a directory
        std::fs::write(dir.path().join(".claude"), "not a directory").unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    // Full roundtrip: install → is_installed → uninstall → !is_installed

    #[test]
    fn test_full_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        // Not installed initially
        assert!(!hook.is_installed(dir.path()));

        // Install
        let count = hook.install(dir.path()).unwrap();
        assert_eq!(count, 7);
        assert!(hook.is_installed(dir.path()));

        // Idempotent install
        let count2 = hook.install(dir.path()).unwrap();
        assert_eq!(count2, 0);
        assert!(hook.is_installed(dir.path()));

        // Uninstall
        hook.uninstall(dir.path()).unwrap();
        assert!(!hook.is_installed(dir.path()));

        // Reinstall
        let count3 = hook.install(dir.path()).unwrap();
        assert_eq!(count3, 7);
        assert!(hook.is_installed(dir.path()));
    }
}

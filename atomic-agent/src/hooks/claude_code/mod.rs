//! Claude Code hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing, installation into `.claude/settings.json`,
//! and presence detection via the `.claude/` directory.

mod parse;
mod settings;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

use super::AgentHook;

pub(crate) use settings::ClaudeHooks;

#[cfg(test)]
pub(crate) use settings::{
    add_hook_to_matcher, ensure_deny_rule, hook_command_exists, is_atomic_hook,
    remove_atomic_hooks, remove_deny_rule, ClaudeHookEntry, ClaudeHookMatcher,
};

// Constants

/// The directory where Claude Code stores per-project settings.
const CLAUDE_DIR: &str = ".claude";

/// The settings file that Claude Code reads hooks from.
const SETTINGS_FILE: &str = "settings.json";

/// Command prefix used to identify Atomic hooks in the settings file.
pub(crate) const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks claude-code";

/// Permission deny rule to prevent Claude from reading Atomic metadata.
#[cfg(test)]
pub(crate) const METADATA_DENY_RULE: &str = "Read(./.atomic/metadata/**)";

// ============================================================================
// CLAUDE CODE HOOK
// ============================================================================

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
    pub fn global_settings_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(CLAUDE_DIR).join(SETTINGS_FILE))
    }

    /// Install hooks globally to `~/.claude/settings.json`.
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

        let (mut raw, mut hooks) = settings::read_settings(&settings_path)?;

        // If forcing, remove existing atomic hooks first
        if force {
            settings::remove_atomic_hooks(&mut hooks.session_start);
            settings::remove_atomic_hooks(&mut hooks.session_end);
            settings::remove_atomic_hooks(&mut hooks.stop);
            settings::remove_atomic_hooks(&mut hooks.user_prompt_submit);
            settings::remove_atomic_hooks(&mut hooks.pre_tool_use);
            settings::remove_atomic_hooks(&mut hooks.post_tool_use);
        }

        let count = install_hooks_into(&mut hooks, &settings_path)?;

        // Add permissions.deny rule
        let permissions_changed = settings::ensure_deny_rule(&mut raw);

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
        settings::write_settings(&settings_path, &raw)?;

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

        let (mut raw, mut hooks) = settings::read_settings(&settings_path)?;

        settings::remove_atomic_hooks(&mut hooks.session_start);
        settings::remove_atomic_hooks(&mut hooks.session_end);
        settings::remove_atomic_hooks(&mut hooks.stop);
        settings::remove_atomic_hooks(&mut hooks.user_prompt_submit);
        settings::remove_atomic_hooks(&mut hooks.pre_tool_use);
        settings::remove_atomic_hooks(&mut hooks.post_tool_use);

        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);
        settings::write_settings(&settings_path, &raw)?;

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

        match settings::read_settings(&settings_path) {
            Ok((_, hooks)) => {
                settings::has_any_atomic_hook(&hooks.session_start)
                    || settings::has_any_atomic_hook(&hooks.session_end)
                    || settings::has_any_atomic_hook(&hooks.stop)
                    || settings::has_any_atomic_hook(&hooks.user_prompt_submit)
                    || settings::has_any_atomic_hook(&hooks.pre_tool_use)
                    || settings::has_any_atomic_hook(&hooks.post_tool_use)
            }
            Err(_) => false,
        }
    }
}

impl Default for ClaudeCodeHook {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// AGENT HOOK TRAIT IMPL
// ============================================================================

impl AgentHook for ClaudeCodeHook {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn display_name(&self) -> &str {
        "Claude Code"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        parse::parse_hook_event(self, hook_type, input)
    }

    fn install(&self, repo_root: &Path) -> AgentResult<usize> {
        let settings_path = Self::settings_path(repo_root);
        let (mut raw, mut hooks) = settings::read_settings(&settings_path)?;

        let count = install_hooks_into(&mut hooks, &settings_path)?;

        // Add permissions.deny rule to prevent Claude from reading metadata
        let permissions_changed = settings::ensure_deny_rule(&mut raw);

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
        settings::write_settings(&settings_path, &raw)?;

        Ok(count)
    }

    fn uninstall(&self, repo_root: &Path) -> AgentResult<()> {
        let settings_path = Self::settings_path(repo_root);

        if !settings_path.exists() {
            return Ok(()); // Nothing to uninstall
        }

        let (mut raw, mut hooks) = settings::read_settings(&settings_path)?;

        // Remove all Atomic hooks from every hook list
        settings::remove_atomic_hooks(&mut hooks.session_start);
        settings::remove_atomic_hooks(&mut hooks.session_end);
        settings::remove_atomic_hooks(&mut hooks.stop);
        settings::remove_atomic_hooks(&mut hooks.user_prompt_submit);
        settings::remove_atomic_hooks(&mut hooks.pre_tool_use);
        settings::remove_atomic_hooks(&mut hooks.post_tool_use);

        // Remove the metadata deny rule from permissions
        settings::remove_deny_rule(&mut raw);

        // Serialize hooks back
        let hooks_val = serde_json::to_value(&hooks).map_err(|e| AgentError::ConfigError {
            operation: "serialize hooks".to_string(),
            path: settings_path.clone(),
            reason: e.to_string(),
        })?;
        raw.insert("hooks".to_string(), hooks_val);
        settings::write_settings(&settings_path, &raw)?;

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
            "post-tool",
        ]
    }
}

// ============================================================================
// HOOK MANIPULATION HELPERS
// ============================================================================

/// The hook definitions: (label, matcher, verb).
const HOOK_DEFS: &[(&str, &str, &str)] = &[
    ("session-start", "", "session-start"),
    ("session-end", "", "session-end"),
    ("stop", "", "stop"),
    ("user-prompt-submit", "", "user-prompt-submit"),
    ("pre-task", "Task", "pre-task"),
    ("post-task", "Task", "post-task"),
    ("post-todo", "TodoWrite", "post-todo"),
    ("post-tool", "", "post-tool"),
];

/// Install all hook definitions into a `ClaudeHooks` struct.
///
/// Returns the number of newly installed hooks.
fn install_hooks_into(hooks: &mut ClaudeHooks, _settings_path: &Path) -> AgentResult<usize> {
    let mut count = 0;
    for (_label, matcher, verb) in HOOK_DEFS {
        let command =
            crate::hooks::guarded_hook_command(&format!("{} {}", ATOMIC_HOOK_PREFIX, verb));

        let matchers = match *verb {
            "session-start" => &mut hooks.session_start,
            "session-end" => &mut hooks.session_end,
            "stop" => &mut hooks.stop,
            "user-prompt-submit" => &mut hooks.user_prompt_submit,
            "pre-task" => &mut hooks.pre_tool_use,
            "post-task" | "post-todo" | "post-tool" => &mut hooks.post_tool_use,
            _ => continue,
        };

        if !settings::hook_command_exists(matchers, matcher, &command) {
            settings::add_hook_to_matcher(matchers, matcher, &command);
            count += 1;
        }
    }
    Ok(count)
}

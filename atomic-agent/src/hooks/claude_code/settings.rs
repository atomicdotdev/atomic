//! Settings file types and hook manipulation helpers for Claude Code.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};

pub(super) const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks claude-code";
pub(super) const METADATA_DENY_RULE: &str = "Read(./.atomic/metadata/**)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeHookEntry {
    #[serde(rename = "type")]
    pub hook_type: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeHookMatcher {
    #[serde(default)]
    pub matcher: String,
    pub hooks: Vec<ClaudeHookEntry>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ClaudeHooks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_start: Vec<ClaudeHookMatcher>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_end: Vec<ClaudeHookMatcher>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<ClaudeHookMatcher>,

    #[serde(
        default,
        rename = "UserPromptSubmit",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub user_prompt_submit: Vec<ClaudeHookMatcher>,

    #[serde(default, rename = "PreToolUse", skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<ClaudeHookMatcher>,

    #[serde(default, rename = "PostToolUse", skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<ClaudeHookMatcher>,
}

pub(crate) fn read_settings(
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

    let raw: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&data).map_err(|e| AgentError::ConfigError {
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

pub(crate) fn write_settings(
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

    let mut output = serde_json::to_string_pretty(raw).map_err(|e| AgentError::ConfigError {
        operation: "serialize".to_string(),
        path: settings_path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if !output.ends_with('\n') {
        output.push('\n');
    }

    std::fs::write(settings_path, output.as_bytes()).map_err(|e| AgentError::ConfigError {
        operation: "write".to_string(),
        path: settings_path.to_path_buf(),
        reason: e.to_string(),
    })
}

pub(crate) fn hook_command_exists(
    matchers: &[ClaudeHookMatcher],
    matcher_name: &str,
    command: &str,
) -> bool {
    matchers.iter().any(|matcher| {
        matcher.matcher == matcher_name && matcher.hooks.iter().any(|h| h.command == command)
    })
}

pub(crate) fn add_hook_to_matcher(
    matchers: &mut Vec<ClaudeHookMatcher>,
    matcher_name: &str,
    command: &str,
) {
    let entry = ClaudeHookEntry {
        hook_type: "command".to_string(),
        command: command.to_string(),
    };

    if let Some(matcher) = matchers.iter_mut().find(|m| m.matcher == matcher_name) {
        matcher.hooks.push(entry);
        return;
    }

    matchers.push(ClaudeHookMatcher {
        matcher: matcher_name.to_string(),
        hooks: vec![entry],
    });
}

pub(crate) fn has_any_atomic_hook(matchers: &[ClaudeHookMatcher]) -> bool {
    matchers
        .iter()
        .any(|m| m.hooks.iter().any(|h| is_atomic_hook(&h.command)))
}

pub(crate) fn is_atomic_hook(command: &str) -> bool {
    command.contains(ATOMIC_HOOK_PREFIX)
}

pub(crate) fn remove_atomic_hooks(matchers: &mut Vec<ClaudeHookMatcher>) {
    for matcher in matchers.iter_mut() {
        matcher.hooks.retain(|h| !is_atomic_hook(&h.command));
    }
    matchers.retain(|m| !m.hooks.is_empty());
}

pub(crate) fn ensure_deny_rule(raw: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let permissions = raw
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::json!({}));

    let Some(permissions_obj) = permissions.as_object_mut() else {
        return false;
    };

    let deny = permissions_obj
        .entry("deny".to_string())
        .or_insert_with(|| serde_json::json!([]));

    let Some(deny_arr) = deny.as_array_mut() else {
        return false;
    };

    let rule_value = serde_json::Value::String(METADATA_DENY_RULE.to_string());
    if deny_arr.contains(&rule_value) {
        return false;
    }

    deny_arr.push(rule_value);
    true
}

pub(crate) fn remove_deny_rule(raw: &mut serde_json::Map<String, serde_json::Value>) {
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

    if deny_arr.is_empty() {
        permissions_obj.remove("deny");
    }
    if permissions_obj.is_empty() {
        raw.remove("permissions");
    }
}

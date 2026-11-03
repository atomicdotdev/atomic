//! Read-only inspection of hook configuration formats understood by Atomic.

use serde_json::Value;

/// Inspect an already parsed settings document. Unsupported formats and
/// malformed hook structures return an error rather than implying absence.
///
/// # Errors
/// Returns a static, content-free explanation when the configuration cannot be
/// interpreted reliably.
pub fn json_hooks_active(
    agent: &str,
    root: &Value,
    hooks_key: &str,
    prefix: &str,
) -> Result<bool, &'static str> {
    let events: &[&str] = match agent {
        "claude-code" | "codex" => &[
            "SessionStart",
            "SessionEnd",
            "Stop",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
        ],
        "gemini-cli" => &[
            "SessionStart",
            "SessionEnd",
            "BeforeAgent",
            "AfterAgent",
            "BeforeTool",
            "AfterTool",
        ],
        _ => return Err("agent hook format is not supported for inspection"),
    };
    if hooks_key != "hooks" || prefix != format!("atomic agent hooks {agent}") {
        return Err("unsupported hook key or command prefix");
    }
    let root = root.as_object().ok_or("settings root is not an object")?;
    if agent == "gemini-cli" {
        for key in ["hooksConfig", "tools"] {
            if root.get(key).is_some_and(|value| !value.is_object()) {
                return Err("hook settings are not an object");
            }
        }
        if let Some(disabled) = root.get("hooksConfig").and_then(|v| v.get("disabled")) {
            if !disabled
                .as_array()
                .is_some_and(|names| names.iter().all(Value::is_string))
            {
                return Err("disabled hook names are not an array of strings");
            }
        }
    }
    let flags = match agent {
        "claude-code" => [root.get("disableAllHooks"), None],
        "gemini-cli" => [
            root.get("hooksConfig").and_then(|v| v.get("enabled")),
            root.get("tools").and_then(|v| v.get("enableHooks")),
        ],
        _ => [None, None],
    };
    for flag in flags.into_iter().flatten() {
        if !flag.is_boolean() {
            return Err("hook enablement flag is not a boolean");
        }
    }
    if globally_disabled(agent, root) {
        return Ok(false);
    }
    let Some(hooks) = root.get(hooks_key) else {
        return Ok(false);
    };
    let hooks = hooks.as_object().ok_or("hooks are not an object")?;
    if hooks.iter().any(|(event, entries)| {
        !events.contains(&event.as_str()) && entries.to_string().contains(prefix)
    }) {
        return Err("hook event cannot be verified");
    }
    let mut active = false;
    for event in events {
        let Some(groups) = hooks.get(*event) else {
            continue;
        };
        let groups = groups.as_array().ok_or("hook event is not an array")?;
        for group in groups {
            let group = group.as_object().ok_or("hook group is not an object")?;
            if disabled(group)? {
                continue;
            }
            if group.get("matcher").is_some_and(|v| !v.is_string()) {
                return Err("hook matcher is not a string");
            }
            let entries = group
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or("hook group has no hook array")?;
            for entry in entries {
                let entry = entry.as_object().ok_or("hook entry is not an object")?;
                if disabled(entry)? {
                    continue;
                }
                if agent == "gemini-cli" {
                    if let Some(name) = entry.get("name").and_then(Value::as_str) {
                        if root
                            .get("hooksConfig")
                            .and_then(|v| v.get("disabled"))
                            .and_then(Value::as_array)
                            .is_some_and(|names| names.iter().any(|v| v.as_str() == Some(name)))
                        {
                            continue;
                        }
                    }
                }
                let hook_type = entry
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or("hook type is missing or invalid")?;
                if hook_type != "command" {
                    continue;
                }
                let command = entry
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or("command hook has no command")?;
                if is_invocation(command, prefix, agent, event) {
                    active = true;
                } else if command.contains(prefix) {
                    return Err("hook command cannot be verified");
                }
            }
        }
    }
    Ok(active)
}

/// Whether this settings object disables all hooks at its scope.
pub fn globally_disabled(agent: &str, root: &serde_json::Map<String, Value>) -> bool {
    match agent {
        "claude-code" => root.get("disableAllHooks") == Some(&Value::Bool(true)),
        "gemini-cli" => {
            root.get("hooksConfig").and_then(|v| v.get("enabled")) == Some(&Value::Bool(false))
                || root.get("tools").and_then(|v| v.get("enableHooks")) == Some(&Value::Bool(false))
        }
        _ => false,
    }
}

fn disabled(entry: &serde_json::Map<String, Value>) -> Result<bool, &'static str> {
    for flag in [entry.get("disabled"), entry.get("enabled")]
        .into_iter()
        .flatten()
    {
        if !flag.is_boolean() {
            return Err("hook enablement flag is not a boolean");
        }
    }
    Ok(entry.get("disabled") == Some(&Value::Bool(true))
        || entry.get("enabled") == Some(&Value::Bool(false)))
}

fn is_invocation(command: &str, prefix: &str, agent: &str, event: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let command = command.trim();
    let guard = "test -d .atomic || test -f .atomic-sandbox && ";
    let (command, guarded) = match command.strip_prefix(guard) {
        Some(command) => (command, true),
        None => (command, false),
    };
    let command = if guarded {
        let Some(command) = command.strip_suffix(" || true") else {
            return false;
        };
        command
    } else {
        command
    };
    let Some(rest) = command.strip_prefix(prefix) else {
        return false;
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let mut args = rest.split_whitespace();
    let Some(verb) = args.next() else {
        return false;
    };
    let recognized = matches!(
        (agent, event, verb),
        (
            "claude-code" | "codex" | "gemini-cli",
            "SessionStart",
            "session-start"
        ) | (
            "claude-code" | "codex" | "gemini-cli",
            "SessionEnd",
            "session-end"
        ) | ("claude-code" | "codex", "Stop", "stop")
            | (
                "claude-code" | "codex",
                "UserPromptSubmit",
                "user-prompt-submit"
            )
            | ("claude-code", "PreToolUse", "pre-task")
            | (
                "claude-code",
                "PostToolUse",
                "post-task" | "post-todo" | "post-tool"
            )
            | ("codex", "PreToolUse", "pre-tool")
            | ("codex", "PostToolUse", "post-tool")
            | ("gemini-cli", "BeforeAgent", "before-agent")
            | ("gemini-cli", "AfterAgent", "after-agent")
            | ("gemini-cli", "BeforeTool", "before-tool")
            | ("gemini-cli", "AfterTool", "after-tool")
    );
    if !recognized {
        return false;
    }
    match (args.next(), args.next()) {
        (None, None) => true,
        (Some("--foreground"), None) => agent == "codex" && verb == "stop",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_active_command_entries_count() {
        let prefix = "atomic agent hooks claude-code";
        let active = |command: &str| json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":command}]}]}});
        for command in ["atomic agent hooks claude-code stop", "test -d .atomic || test -f .atomic-sandbox && atomic agent hooks claude-code stop || true"] {
            assert_eq!(json_hooks_active("claude-code", &active(command), "hooks", prefix), Ok(true));
        }
        for root in [
            json!({"notes":prefix,"hooks":{}}),
            json!({"disableAllHooks":true,"hooks":active(prefix)["hooks"]}),
            json!({"hooks":{"Stop":[{"disabled":true,"hooks":[{"type":"command","command":format!("{prefix} stop")}]}]}}),
        ] {
            assert_eq!(
                json_hooks_active("claude-code", &root, "hooks", prefix),
                Ok(false),
                "{root}"
            );
        }
        for command in [
            format!("echo {prefix} stop"),
            "atomic agent hooks claude-code-not stop".to_string(),
        ] {
            assert!(json_hooks_active("claude-code", &active(&command), "hooks", prefix).is_err());
        }
    }

    #[test]
    fn supported_formats_and_disabled_states() {
        for (agent, event, verb) in [
            ("claude-code", "Stop", "stop"),
            ("codex", "Stop", "stop"),
            ("gemini-cli", "AfterAgent", "after-agent"),
        ] {
            let prefix = format!("atomic agent hooks {agent}");
            let mut root = json!({"hooks":{event:[{"hooks":[{"type":"command","name":"atomic-test","command":format!("{prefix} {verb}")}]}]}});
            assert_eq!(
                json_hooks_active(agent, &root, "hooks", &prefix),
                Ok(true),
                "{agent}"
            );
            if agent == "claude-code" {
                root["tools"] = json!({"enableHooks":false});
                assert_eq!(json_hooks_active(agent, &root, "hooks", &prefix), Ok(true));
            }
            root["hooks"][event][0]["hooks"][0]["enabled"] = json!(false);
            assert_eq!(
                json_hooks_active(agent, &root, "hooks", &prefix),
                Ok(false),
                "{agent}"
            );
            root["hooks"][event][0]["hooks"][0]["enabled"] = json!(true);
            assert!(json_hooks_active(agent, &root, "other", &prefix).is_err());
            if agent == "gemini-cli" {
                root["hooksConfig"] = json!({"disabled":["atomic-test"]});
                assert_eq!(json_hooks_active(agent, &root, "hooks", &prefix), Ok(false));
            }
        }
        assert!(json_hooks_active(
            "opencode",
            &json!({}),
            "hooks",
            "atomic agent hooks opencode"
        )
        .is_err());
        assert!(json_hooks_active(
            "codex",
            &json!({"hooks":[]}),
            "hooks",
            "atomic agent hooks codex"
        )
        .is_err());
        let incomplete = json!({"hooks":{"Stop":[{"hooks":[
            {"type":"command","command":"atomic agent hooks claude-code stop"},
            {"type":"command"}
        ]}]}});
        assert!(json_hooks_active(
            "claude-code",
            &incomplete,
            "hooks",
            "atomic agent hooks claude-code"
        )
        .is_err());
    }

    #[test]
    fn built_in_installers_produce_recognised_hooks() {
        use crate::hooks::AgentHook;

        let home = tempfile::tempdir().unwrap();
        let claude = crate::hooks::claude_code::ClaudeCodeHook::new();
        claude.install(home.path()).unwrap();
        let gemini = crate::hooks::gemini_cli::GeminiCliHook::new();
        gemini.install(home.path()).unwrap();
        let codex = crate::hooks::codex::CodexHook::new();
        codex
            .install_at(&home.path().join(".codex/hooks.json"), false)
            .unwrap();
        for (agent, relative) in [
            ("claude-code", ".claude/settings.json"),
            ("gemini-cli", ".gemini/settings.json"),
            ("codex", ".codex/hooks.json"),
        ] {
            let root: Value =
                serde_json::from_slice(&std::fs::read(home.path().join(relative)).unwrap())
                    .unwrap();
            assert_eq!(
                json_hooks_active(
                    agent,
                    &root,
                    "hooks",
                    &format!("atomic agent hooks {agent}")
                ),
                Ok(true),
                "{agent}"
            );
        }
    }
}

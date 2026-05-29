//! Tests for the Claude Code hook adapter.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::event::HookType;
    use crate::hooks::claude_code::{
        add_hook_to_matcher, ensure_deny_rule, hook_command_exists, is_atomic_hook,
        remove_atomic_hooks, remove_deny_rule, ClaudeCodeHook, ClaudeHookEntry, ClaudeHookMatcher,
        ATOMIC_HOOK_PREFIX, METADATA_DENY_RULE,
    };
    use crate::hooks::AgentHook;
    use std::path::PathBuf;

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
        assert_eq!(verbs.len(), 8);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"user-prompt-submit"));
        assert!(verbs.contains(&"pre-task"));
        assert!(verbs.contains(&"post-task"));
        assert!(verbs.contains(&"post-todo"));
        assert!(verbs.contains(&"post-tool"));
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
            crate::error::AgentError::HookInputEmpty { agent, hook_type } => {
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
            crate::error::AgentError::HookParseFailed { .. }
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
        // Bare format (legacy)
        assert!(is_atomic_hook("atomic agent hooks claude-code stop"));
        assert!(is_atomic_hook(
            "atomic agent hooks claude-code user-prompt-submit"
        ));
        // Guarded format (current)
        assert!(is_atomic_hook(
            "test -d .atomic && atomic agent hooks claude-code stop || true"
        ));
        assert!(is_atomic_hook(
            "test -d .atomic && atomic agent hooks claude-code user-prompt-submit || true"
        ));
        // Non-atomic commands
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

    #[test]
    fn test_remove_atomic_hooks_guarded_format() {
        let mut matchers = vec![ClaudeHookMatcher {
            matcher: "".to_string(),
            hooks: vec![
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "some other hook".to_string(),
                },
                ClaudeHookEntry {
                    hook_type: "command".to_string(),
                    command: "test -d .atomic && atomic agent hooks claude-code stop || true"
                        .to_string(),
                },
            ],
        }];

        remove_atomic_hooks(&mut matchers);

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].hooks.len(), 1);
        assert_eq!(matchers[0].hooks[0].command, "some other hook");
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

        // Should install 8 hooks
        assert_eq!(count, 8);

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
        assert!(data.contains("atomic agent hooks claude-code post-tool"));
        assert!(data.contains(METADATA_DENY_RULE));
    }

    #[test]
    fn test_install_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();

        let count1 = hook.install(dir.path()).unwrap();
        assert_eq!(count1, 8);

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
        assert_eq!(count, 8);
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
        assert_eq!(count3, 8);
        assert!(hook.is_installed(dir.path()));
    }
}

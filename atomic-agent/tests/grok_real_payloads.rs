//! Hook payloads captured from a real Grok Build 1.0.77 session.
//!
//! Grok sends its own camelCase key and Claude's snake_case key side by
//! side for most top-level fields. The fixtures keep every key, key order
//! and value Grok sent; only machine-specific paths are rewritten.

use std::path::Path;

use atomic_agent::event::{HookType, TurnEvent};
use atomic_agent::hooks::AgentRegistry;
use serde_json::Value;

const SESSION_ID: &str = "01a1d576-264a-7530-a25e-d209bd34a860";
const TRANSCRIPT_PATH: &str = "/Users/you/.grok/sessions/%2FUsers%2Fyou%2Fproject/01a1d576-264a-7530-a25e-d209bd34a860/updates.jsonl";

const SESSION_START: &str = include_str!("fixtures/grok-1.0.77/session_start.json");
const USER_PROMPT_SUBMIT: &str = include_str!("fixtures/grok-1.0.77/user_prompt_submit.json");
const PRE_TOOL_USE: &str = include_str!("fixtures/grok-1.0.77/pre_tool_use.json");
const POST_TOOL_USE: &str = include_str!("fixtures/grok-1.0.77/post_tool_use.json");
const STOP_END_TURN: &str = include_str!("fixtures/grok-1.0.77/stop_end_turn.json");
const STOP_SHUTDOWN: &str = include_str!("fixtures/grok-1.0.77/stop_shutdown.json");
const SESSION_END: &str = include_str!("fixtures/grok-1.0.77/session_end.json");

fn parse(hook_type: HookType, payload: &str) -> TurnEvent {
    let registry = AgentRegistry::with_defaults();
    let grok = registry.require("grok").unwrap();
    grok.parse_event(hook_type, payload.as_bytes())
        .unwrap_or_else(|error| panic!("grok {hook_type}: {error}"))
}

fn raw_str<'a>(event: &'a TurnEvent, key: &str) -> Option<&'a str> {
    event.raw_json.as_ref()?.get(key).and_then(Value::as_str)
}

#[test]
fn every_real_payload_parses() {
    let payloads = [
        (HookType::SessionStart, SESSION_START),
        (HookType::TurnStart, USER_PROMPT_SUBMIT),
        (HookType::PreToolUse, PRE_TOOL_USE),
        (HookType::PostToolUse, POST_TOOL_USE),
        (HookType::TurnEnd, STOP_END_TURN),
        (HookType::TurnEnd, STOP_SHUTDOWN),
        (HookType::SessionEnd, SESSION_END),
    ];
    for (hook_type, payload) in payloads {
        let event = parse(hook_type, payload);
        assert_eq!(event.session_id, SESSION_ID, "{hook_type}");
        assert_eq!(event.event_type, hook_type);
        assert_eq!(raw_str(&event, "provider"), Some("xai"), "{hook_type}");
    }
}

#[test]
fn real_payloads_keep_both_spellings_in_raw_json() {
    let event = parse(HookType::SessionStart, SESSION_START);
    assert_eq!(raw_str(&event, "hookEventName"), Some("session_start"));
    assert_eq!(raw_str(&event, "hook_event_name"), Some("SessionStart"));
    assert_eq!(raw_str(&event, "sessionId"), Some(SESSION_ID));
    assert_eq!(raw_str(&event, "session_id"), Some(SESSION_ID));
}

#[test]
fn real_user_prompt_submit_carries_the_prompt() {
    let event = parse(HookType::TurnStart, USER_PROMPT_SUBMIT);
    assert_eq!(
        event.prompt.as_deref(),
        Some("Read the file a.txt and tell me its first line. Do not modify anything.")
    );
}

#[test]
fn real_pre_tool_use_carries_the_tool_call() {
    let event = parse(HookType::PreToolUse, PRE_TOOL_USE);
    assert_eq!(event.tool_name.as_deref(), Some("read_file"));
    assert_eq!(
        event.tool_use_id.as_deref(),
        Some("call-e15f7982-e832-4e29-a8a6-0ea59a23b34b-0")
    );
    assert_eq!(
        event.transcript_path.as_deref(),
        Some(Path::new(TRANSCRIPT_PATH))
    );
}

#[test]
fn real_post_tool_use_carries_the_tool_result() {
    let event = parse(HookType::PostToolUse, POST_TOOL_USE);
    assert_eq!(event.tool_name.as_deref(), Some("read_file"));
    assert_eq!(raw_str(&event, "status"), Some("completed"));
    assert_eq!(
        raw_str(&event, "file_path"),
        Some("/Users/you/project/a.txt")
    );
    let raw = event.raw_json.as_ref().unwrap();
    assert_eq!(raw.get("toolResult"), raw.get("tool_response"));
    assert!(raw_str(&event, "tool_output").is_some_and(|output| output.contains("first line")));
}

#[test]
fn real_stop_maps_the_reason_onto_finish_reason() {
    let end_turn = parse(HookType::TurnEnd, STOP_END_TURN);
    assert_eq!(raw_str(&end_turn, "finish_reason"), Some("stop"));
    assert_eq!(
        raw_str(&end_turn, "last_assistant_message"),
        Some("The first line of `a.txt` is:\n\n```\nfirst line\n```")
    );

    let shutdown = parse(HookType::TurnEnd, STOP_SHUTDOWN);
    assert_eq!(raw_str(&shutdown, "finish_reason"), Some("session-end"));
}

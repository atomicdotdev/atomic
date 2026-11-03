//! Source-time contracts for every registered hook adapter.

use atomic_agent::error::AgentError;
use atomic_agent::event::{HookType, ProvenanceJournalEnvelope};
use atomic_agent::hooks::{AgentHook, AgentRegistry};
use chrono::Utc;
use serde_json::{json, Value};

fn input(timestamp: Option<Value>) -> Vec<u8> {
    let mut raw = json!({
        "session_id": "source-time",
        "conversation_id": "source-time",
        "taskId": "source-time",
        "thread_id": "source-time",
        "prompt": "Keep the source time",
        "tool_name": "read",
        "tool_use_id": "tool-1",
        // These are user data, not the hook envelope's timestamp.
        "tool_input": {"timestamp": "not an event timestamp"},
        "extra": {"timestamp": "not an event timestamp"},
    });
    if let Some(timestamp) = timestamp {
        raw["timestamp"] = timestamp;
    }
    serde_json::to_vec(&raw).unwrap()
}

fn source_timestamps(agent: &str) -> Vec<Value> {
    match agent {
        "copilot" => vec![
            json!(1_792_920_379_589_i64),
            json!("2026-10-25T09:26:19.589Z"),
        ],
        "cline" => vec![json!("1792920379589")],
        _ => vec![json!("2026-10-25T09:26:19.589Z")],
    }
}

fn for_each_hook(mut test: impl FnMut(&dyn AgentHook, HookType)) {
    let registry = AgentRegistry::with_defaults();
    for name in registry.list() {
        let agent = registry.require(name).unwrap();
        for hook_type in agent.supported_hooks() {
            test(agent, hook_type);
        }
    }
}

#[test]
fn every_adapter_preserves_source_time_in_events_and_journal_envelopes() {
    for_each_hook(|agent, hook_type| {
        for timestamp in source_timestamps(agent.name()) {
            let event = agent
                .parse_event(hook_type, &input(Some(timestamp.clone())))
                .unwrap_or_else(|error| panic!("{} {hook_type}: {error}", agent.name()));
            assert_eq!(
                event.timestamp.timestamp_millis(),
                1_792_920_379_589,
                "{} {hook_type}",
                agent.name()
            );
            assert_eq!(
                event.raw_json.as_ref().unwrap()["timestamp"],
                timestamp,
                "{} {hook_type}: raw timestamp was rewritten",
                agent.name()
            );
            let envelope = ProvenanceJournalEnvelope::from_turn_event("source-event", 1, 1, &event);
            assert_eq!(
                envelope.timestamp_ms,
                1_792_920_379_589,
                "{} {hook_type}: journal lost source time",
                agent.name()
            );
        }
    });
}

#[test]
fn every_adapter_uses_receipt_time_for_missing_or_null_timestamps_only() {
    for_each_hook(|agent, hook_type| {
        for timestamp in [None, Some(Value::Null)] {
            let before = Utc::now();
            let event = agent
                .parse_event(hook_type, &input(timestamp))
                .unwrap_or_else(|error| panic!("{} {hook_type}: {error}", agent.name()));
            let after = Utc::now();
            assert!(
                (before..=after).contains(&event.timestamp),
                "{} {hook_type}: missing timestamp did not use receipt time",
                agent.name()
            );
        }
    });
}

#[test]
fn every_adapter_rejects_malformed_supplied_timestamps_with_context() {
    for_each_hook(|agent, hook_type| {
        for timestamp in [
            json!(""),
            json!("not a timestamp"),
            json!("2026-13-25T09:26:19.589Z"),
            json!(true),
            json!({}),
            json!([]),
        ] {
            let error = agent
                .parse_event(hook_type, &input(Some(timestamp.clone())))
                .expect_err("invalid source time must not silently become receipt time");
            match error {
                AgentError::HookParseFailed {
                    agent: name,
                    hook_type: kind,
                    reason,
                } => {
                    assert_eq!(name, agent.name());
                    assert_eq!(kind, hook_type.as_str());
                    assert!(
                        reason.contains("timestamp"),
                        "missing timestamp context: {reason}"
                    );
                }
                error => panic!(
                    "{} {hook_type} {timestamp}: unexpected error {error}",
                    agent.name()
                ),
            }
        }
    });
}

#[test]
fn rfc3339_adapters_preserve_offsets_and_submillisecond_precision() {
    for_each_hook(|agent, hook_type| {
        if agent.name() == "cline" {
            return;
        }
        for timestamp in [
            "2026-10-25T11:26:19.589123456+02:00",
            "2026-10-25T09:26:19.589123456Z",
        ] {
            let event = agent
                .parse_event(hook_type, &input(Some(json!(timestamp))))
                .unwrap();
            assert_eq!(
                event.timestamp.timestamp_millis(),
                1_792_920_379_589,
                "{} {hook_type}",
                agent.name()
            );
            assert_eq!(
                event.timestamp.timestamp_subsec_nanos(),
                589_123_456,
                "{} {hook_type}",
                agent.name()
            );
        }
    });
}

#[test]
fn explicit_epoch_milliseconds_are_never_guessed_from_magnitude() {
    let registry = AgentRegistry::with_defaults();
    for name in ["copilot", "cline"] {
        let agent = registry.require(name).unwrap();
        for hook_type in agent.supported_hooks() {
            for millis in [-1_i64, 0, 1, 1_700_000_000, 1_792_920_379_589] {
                let timestamp = if name == "cline" {
                    json!(millis.to_string())
                } else {
                    json!(millis)
                };
                let event = agent
                    .parse_event(hook_type, &input(Some(timestamp)))
                    .unwrap();
                assert_eq!(
                    event.timestamp.timestamp_millis(),
                    millis,
                    "{name} {hook_type}"
                );
                let envelope =
                    ProvenanceJournalEnvelope::from_turn_event("source-event", 1, 1, &event);
                assert_eq!(envelope.timestamp_ms, millis, "{name} {hook_type}");
            }
        }
    }
}

#[test]
fn epoch_adapters_reject_overflow_and_nonintegral_values() {
    let registry = AgentRegistry::with_defaults();
    for name in ["copilot", "cline"] {
        let agent = registry.require(name).unwrap();
        for hook_type in agent.supported_hooks() {
            for value in [json!(i64::MAX), json!(u64::MAX), json!(1.5)] {
                let timestamp = if name == "cline" {
                    json!(value.to_string())
                } else {
                    value
                };
                assert!(
                    matches!(
                        agent.parse_event(hook_type, &input(Some(timestamp))),
                        Err(AgentError::HookParseFailed { .. })
                    ),
                    "{name} {hook_type}"
                );
            }
        }
    }
}

#[test]
fn adapters_reject_timestamp_representations_outside_their_wire_contract() {
    for_each_hook(|agent, hook_type| {
        let unsupported = match agent.name() {
            "copilot" => json!("1792920379589"), // Not a documented Copilot string encoding.
            "cline" => json!("2026-10-25T09:26:19.589Z"), // Cline strings are epoch milliseconds.
            _ => json!(1_792_920_379_589_i64),   // RFC-3339 contracts do not guess numeric units.
        };
        assert!(
            matches!(
                agent.parse_event(hook_type, &input(Some(unsupported))),
                Err(AgentError::HookParseFailed { .. })
            ),
            "{} {hook_type}",
            agent.name()
        );
    });
}

#[test]
fn every_adapter_keeps_empty_and_malformed_json_errors() {
    for_each_hook(|agent, hook_type| {
        assert!(
            matches!(
                agent.parse_event(hook_type, b""),
                Err(AgentError::HookInputEmpty { .. })
            ),
            "{} {hook_type}",
            agent.name()
        );
        assert!(
            matches!(
                agent.parse_event(hook_type, b"not json"),
                Err(AgentError::HookParseFailed { .. })
            ),
            "{} {hook_type}",
            agent.name()
        );
    });
}

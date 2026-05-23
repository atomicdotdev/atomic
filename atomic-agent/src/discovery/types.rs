//! Shared data types for the `TraceDiscovery` trait.
//!
//! Mirrors the wire-format produced by all discovery adapters (JSONL, JSON,
//! SQLite) into a normalized representation that feeds the provenance import
//! pipeline. Adapters implement [`super::TraceDiscovery`] and emit these types;
//! consumers read only these types, never adapter-specific structs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Backing storage format an adapter reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    /// JSON Lines — one object per line. Used by Claude Code, Codex, Gemini CLI, Copilot.
    Jsonl,
    /// Single JSON document per trace. Used by Cline, Amp, OpenClaw, Pi.
    Json,
    /// SQLite database. Used by Cursor, Hermes, OpenCode.
    Sqlite,
}

/// Normalized event category. Discovery adapters classify each agent event into
/// one of these variants so downstream provenance accumulation is uniform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveredEventType {
    UserMessage,
    AssistantText,
    AssistantThinking,
    ToolCall,
    ToolResult,
    Error,
}

/// Metadata about a single trace (session) found on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredTrace {
    /// Adapter-defined unique id for the trace within this agent.
    pub trace_id: String,
    /// Owning agent id (matches `TraceDiscovery::agent_id()`).
    pub agent_id: String,
    /// Optional human-readable title (often the first user message).
    pub title: Option<String>,
    /// Optional short preview snippet for listing UIs.
    pub preview: Option<String>,
    /// Most recent activity time on the trace.
    pub timestamp: DateTime<Utc>,
    /// Working directory the trace was associated with, if recorded.
    pub directory: Option<PathBuf>,
    /// On-disk path the trace was loaded from (file or DB).
    pub source_path: PathBuf,
}

/// A single normalized event read from a trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredEvent {
    /// Classification of this event within the trace.
    pub event_type: DiscoveredEventType,
    /// Optional speaker role (e.g., "user", "assistant", "system"). Some adapters
    /// store this redundantly with `event_type`; preserve it when available.
    pub role: Option<String>,
    /// Text payload for message/result events.
    pub text: Option<String>,
    /// Tool name for `ToolCall` / `ToolResult` events.
    pub tool_name: Option<String>,
    /// Correlation id linking a `ToolResult` to its preceding `ToolCall`.
    pub tool_call_id: Option<String>,
    /// Model identifier (e.g., "claude-sonnet-4-5") when known.
    pub model_id: Option<String>,
    /// Event timestamp when available. Many adapters only have trace-level ts.
    pub timestamp: Option<DateTime<Utc>>,
    /// Monotonic ordering index within a trace (0-based). Required.
    pub order: u64,
    /// The original adapter-specific JSON payload, preserved for debugging.
    pub raw_json: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_kind_serde_roundtrip() {
        let variants = [StorageKind::Jsonl, StorageKind::Json, StorageKind::Sqlite];
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: StorageKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {:?}", variant);
        }
    }

    #[test]
    fn test_storage_kind_serde_format() {
        assert_eq!(
            serde_json::to_string(&StorageKind::Jsonl).unwrap(),
            "\"jsonl\""
        );
        assert_eq!(
            serde_json::to_string(&StorageKind::Json).unwrap(),
            "\"json\""
        );
        assert_eq!(
            serde_json::to_string(&StorageKind::Sqlite).unwrap(),
            "\"sqlite\""
        );
    }

    #[test]
    fn test_event_type_serde_roundtrip() {
        let variants = [
            DiscoveredEventType::UserMessage,
            DiscoveredEventType::AssistantText,
            DiscoveredEventType::AssistantThinking,
            DiscoveredEventType::ToolCall,
            DiscoveredEventType::ToolResult,
            DiscoveredEventType::Error,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: DiscoveredEventType = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {:?}", variant);
        }
    }

    #[test]
    fn test_event_type_serde_format() {
        assert_eq!(
            serde_json::to_string(&DiscoveredEventType::AssistantText).unwrap(),
            "\"assistant_text\""
        );
    }

    #[test]
    fn test_discovered_trace_serde_roundtrip() {
        let ts = Utc::now();
        let trace = DiscoveredTrace {
            trace_id: "trace-abc-123".to_string(),
            agent_id: "claude-code".to_string(),
            title: Some("Fix the auth bug".to_string()),
            preview: Some("Let me look at the login module...".to_string()),
            timestamp: ts,
            directory: Some(PathBuf::from("/home/user/project")),
            source_path: PathBuf::from("/home/user/.claude/projects/trace.jsonl"),
        };

        let json = serde_json::to_string(&trace).unwrap();
        let parsed: DiscoveredTrace = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.trace_id, trace.trace_id);
        assert_eq!(parsed.agent_id, trace.agent_id);
        assert_eq!(parsed.title, trace.title);
        assert_eq!(parsed.timestamp, trace.timestamp);
    }

    #[test]
    fn test_discovered_event_serde_roundtrip() {
        let event = DiscoveredEvent {
            event_type: DiscoveredEventType::ToolCall,
            role: Some("assistant".to_string()),
            text: None,
            tool_name: Some("Edit".to_string()),
            tool_call_id: Some("tu-999".to_string()),
            model_id: Some("claude-sonnet-4-5".to_string()),
            timestamp: Some(Utc::now()),
            order: 7,
            raw_json: serde_json::json!({"k": "v"}),
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: DiscoveredEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.event_type, event.event_type);
        assert_eq!(parsed.order, event.order);
        assert_eq!(parsed.raw_json, serde_json::json!({"k": "v"}));
    }

    #[test]
    fn test_discovered_trace_optional_fields_none() {
        let ts = Utc::now();
        let trace = DiscoveredTrace {
            trace_id: "trace-minimal".to_string(),
            agent_id: "gemini-cli".to_string(),
            title: None,
            preview: None,
            timestamp: ts,
            directory: None,
            source_path: PathBuf::from("/tmp/trace.json"),
        };

        let json = serde_json::to_string(&trace).unwrap();
        let parsed: DiscoveredTrace = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.trace_id, trace.trace_id);
        assert!(parsed.title.is_none());
        assert!(parsed.preview.is_none());
        assert!(parsed.directory.is_none());
        assert_eq!(parsed.source_path, trace.source_path);
    }
}

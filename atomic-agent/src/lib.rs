//! AI agent integration for Atomic VCS.
//!
//! This crate provides turn-level recording of AI coding agent sessions using
//! Atomic's change system and Watchman's file watching. Each agent turn becomes
//! a single Atomic change with full provenance, session metadata, and optional
//! transcript — all embedded in the change itself so it commutes via patch
//! theory, pushes to remotes automatically, and is available on the server
//! for UI rendering.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         atomic-agent                                    │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Agent Hook (stdin JSON)                                                │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │   hooks::AgentHook::parse_event()                                       │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │   event::TurnEvent                                                      │
//! │       │                                                                 │
//! │       ├──▶ turn::TurnOrchestrator (state machine dispatch)              │
//! │       │         │                                                       │
//! │       │         ├──▶ watcher::FileWatcher::begin_turn() / end_turn()    │
//! │       │         │         │                                             │
//! │       │         │         ▼                                             │
//! │       │         │    event::TurnChanges { modified, added, deleted }    │
//! │       │         │         │                                             │
//! │       │         └─────────┴──▶ record_turn() → Atomic Change           │
//! │       │                            │                                    │
//! │       │                            ▼                                    │
//! │       │                   Change contains:                              │
//! │       │                   • hashed.provenance (tokens, cost, model)     │
//! │       │                   • hashed.metadata (SessionEnvelope)           │
//! │       │                   • unhashed (transcript)                       │
//! │       │                                                                 │
//! │       └──▶ push → server has all session data inside the changes        │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # What This Replaces
//!
//! This crate replaces the Entire CLI (`github.com/entireio/cli`) by absorbing
//! its capabilities into Atomic natively:
//!
//! | Entire CLI Concept | Atomic Agent Replacement |
//! |--------------------|--------------------------|
//! | Shadow git branches | Atomic views (`agent-{session_id}`) |
//! | Orphan metadata branch | Change `metadata` + `unhashed` fields |
//! | Checkpoint IDs | Merkle state hashing |
//! | Git status diffing | Watchman `since`-clock queries |
//! | `entire rewind` | `atomic unrecord` |
//! | Session state in `.git/` | `.atomic/sessions/` |
//!
//! # Modules
//!
//! - [`error`] — Error types for all agent operations
//! - [`event`] — Turn lifecycle event types (`HookType`, `TurnEvent`, `TurnChanges`)
//! - [`hooks`] — Agent hook adapters (`AgentHook` trait, `AgentRegistry`, Claude Code adapter)
//! - [`provenance`] — Causal decision DAG for agent sessions
//! - [`watcher`] — Watchman file watching integration (planned)
//! - [`turn`] — Turn state machine and session management (planned)
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use atomic_agent::hooks::{AgentRegistry, AgentHook};
//! use atomic_agent::event::HookType;
//!
//! // Create the registry with all built-in agents
//! let registry = AgentRegistry::with_defaults();
//!
//! // Look up an agent
//! let claude = registry.require("claude-code").unwrap();
//!
//! // Parse a hook callback from stdin
//! let input = br#"{"session_id": "abc-123", "transcript_path": "/tmp/t.jsonl"}"#;
//! let event = claude.parse_event(HookType::TurnEnd, input).unwrap();
//! assert_eq!(event.session_id, "abc-123");
//! ```

pub mod envelope;
pub mod error;
pub mod event;
pub mod export;
pub mod hooks;
pub mod identity;
pub mod integrations;
pub mod learnings;
pub mod provenance;
pub mod record;
pub mod transcript;
pub mod turn;
pub mod watcher;

// Re-export primary types for convenience
pub use envelope::SessionEnvelope;
pub use error::{AgentError, AgentResult};
pub use event::{HookType, TurnChanges, TurnEvent};
pub use hooks::{AgentHook, AgentRegistry};
pub use provenance::{
    EdgeKind, GraphEdge, GraphNode, GraphStats, NodeKind, ProvenanceAccumulator, SerializedGraph,
};
pub use record::{record_turn, TurnRecordOptions, TurnRecordOutcome};
pub use turn::{
    apply_common_actions, transition, Action, AgentSession, Event, Phase, SessionStore,
    TransitionContext, TransitionResult,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crate_reexports() {
        // Verify that primary types are accessible from the crate root.
        let _: HookType = HookType::TurnStart;
        let _event = TurnEvent::new("test", HookType::TurnEnd);
        let _changes = TurnChanges::new();
        let _registry = AgentRegistry::with_defaults();
        let _: Phase = Phase::Idle;
        let _envelope = SessionEnvelope::builder("s", "a").build();
        let _session = AgentSession::new("s", "a", "A");

        // Provenance graph types
        let _: NodeKind = NodeKind::Goal;
        let _: EdgeKind = EdgeKind::LedTo;
        let _acc = ProvenanceAccumulator::new("test");
    }

    #[test]
    fn test_default_registry_has_claude_code() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("claude-code");
        assert!(
            agent.is_some(),
            "Default registry should include claude-code"
        );
        assert_eq!(agent.unwrap().display_name(), "Claude Code");
    }

    #[test]
    fn test_default_registry_has_cursor() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("cursor");
        assert!(agent.is_some(), "Default registry should include cursor");
        assert_eq!(agent.unwrap().display_name(), "Cursor");
    }

    #[test]
    fn test_default_registry_has_cline() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("cline");
        assert!(agent.is_some(), "Default registry should include cline");
        assert_eq!(agent.unwrap().display_name(), "Cline");
    }

    #[test]
    fn test_default_registry_has_copilot() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("copilot");
        assert!(agent.is_some(), "Default registry should include copilot");
        assert_eq!(agent.unwrap().display_name(), "GitHub Copilot");
    }

    #[test]
    fn test_default_registry_has_codex() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("codex");
        assert!(agent.is_some(), "Default registry should include codex");
        assert_eq!(agent.unwrap().display_name(), "Codex");
    }

    #[test]
    fn test_default_registry_has_hermes() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("hermes");
        assert!(agent.is_some(), "Default registry should include hermes");
        assert_eq!(agent.unwrap().display_name(), "Hermes Agent");
    }

    #[test]
    fn test_default_registry_has_kiro() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("kiro");
        assert!(agent.is_some(), "Default registry should include kiro");
        assert_eq!(agent.unwrap().display_name(), "Kiro");
    }

    #[test]
    fn test_default_registry_has_pi() {
        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("pi");
        assert!(agent.is_some(), "Default registry should include pi");
        assert_eq!(agent.unwrap().display_name(), "Pi");
    }

    #[test]
    fn test_claude_code_parse_roundtrip() {
        let registry = AgentRegistry::with_defaults();
        let claude = registry.require("claude-code").unwrap();

        let input = br#"{"session_id": "test-session", "transcript_path": "/tmp/t.jsonl"}"#;
        let event = claude.parse_event(HookType::TurnEnd, input).unwrap();

        assert_eq!(event.session_id, "test-session");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_claude_code_parse_with_prompt() {
        let registry = AgentRegistry::with_defaults();
        let claude = registry.require("claude-code").unwrap();

        let input = br#"{"session_id": "s1", "prompt": "Fix the bug in auth.rs"}"#;
        let event = claude.parse_event(HookType::TurnStart, input).unwrap();

        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug in auth.rs"));
    }

    #[test]
    fn test_error_types_accessible() {
        let err = AgentError::SessionNotFound {
            session_id: "test".to_string(),
        };
        assert!(err.to_string().contains("test"));
        assert!(err.suggestion().is_none());
    }

    #[test]
    fn test_turn_changes_accessible() {
        let changes =
            TurnChanges::new().with_modified(vec![std::path::PathBuf::from("src/main.rs")]);
        assert_eq!(changes.file_count(), 1);
        assert!(!changes.is_empty());
    }

    #[test]
    fn test_state_machine_accessible() {
        let result = transition(Phase::Idle, Event::TurnStart, TransitionContext::default());
        assert_eq!(result.new_phase, Phase::Active);
        assert!(result.actions.contains(&Action::UpdateInteraction));
    }

    #[test]
    fn test_session_envelope_encode_decode() {
        let envelope = SessionEnvelope::builder("sess-1", "claude-code")
            .turn_number(2)
            .prompt_summary("Fix the bug")
            .files_in_turn(vec!["src/main.rs".to_string()])
            .build();

        let bytes = envelope.encode().unwrap();
        assert!(SessionEnvelope::is_session_envelope(&bytes));

        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.session_id, "sess-1");
        assert_eq!(decoded.turn_number, 2);
        assert_eq!(decoded.agent_name, "claude-code");
        assert_eq!(decoded.prompt_summary.as_deref(), Some("Fix the bug"));
        assert_eq!(decoded.files_in_turn, vec!["src/main.rs"]);
    }

    #[test]
    fn test_session_envelope_not_other_data() {
        // Plain bytes should not be mistaken for an envelope
        assert!(!SessionEnvelope::is_session_envelope(b"hello world"));
        assert!(!SessionEnvelope::is_session_envelope(b""));
        assert!(!SessionEnvelope::is_session_envelope(
            &serde_json::to_vec(&serde_json::json!({"key": "value"})).unwrap()
        ));
    }
}

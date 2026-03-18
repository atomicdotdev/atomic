//! Session data types for Sherpa-enriched provenance.
//!
//! These types are stored in the session tables in the pristine database
//! when a provenance graph has `profile == "sherpa-trace/1.0.0"`.
//! They represent the structured turn data extracted from the Petri net
//! replay log.
//!
//! # Storage Format
//!
//! Values are serialized with [postcard](https://docs.rs/postcard) (binary,
//! compact, no-std compatible). This is the same codec used by
//! [`ProvenanceGraph`](super::provenance_graph::ProvenanceGraph).
//!
//! # Table Layout
//!
//! | Table | Key | Value |
//! |-------|-----|-------|
//! | `SESSION_INTENTS` | `provenance_id: u64` | `IntentEntry` |
//! | `SESSION_TODOS` | `(provenance_id, todo_id_hash): [u8; 16]` | `TodoSnapshot` |
//! | `SESSION_PHASES` | `(provenance_id, phase_hash): [u8; 16]` | `PhaseTimingEntry` |
//! | `SESSION_EVENTS` | `(provenance_id, seq): [u8; 16]` | `SessionEvent` |
//!
//! Composite keys are 16-byte arrays following the same pattern as
//! `STACK_CHANGES`: first 8 bytes = discriminant (u64 LE), second 8 bytes =
//! secondary key (u64 LE or first-8-bytes-of-blake3).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// Intent metadata for a turn.
///
/// Stored in `SESSION_INTENTS` keyed by `provenance_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentEntry {
    /// Short human-readable title for the turn's goal.
    pub title: String,
    /// Longer description of what the agent was asked to do.
    pub description: String,
    /// Zero-based turn index within the session.
    pub turn_id: u32,
    /// LLM model identifier (e.g. `"claude-sonnet-4-20250514"`).
    pub model: String,
    /// Unique session identifier from the agent framework.
    pub session_id: String,
    /// Outcome tag (e.g. `"success"`, `"failure"`, `"partial"`).
    pub outcome: String,
    /// Total token count (prompt + completion) for the turn.
    pub total_tokens: u64,
    /// Total estimated cost in USD for the turn.
    pub total_cost_usd: f64,
}

/// Final state of a todo item at the end of a turn.
///
/// Stored in `SESSION_TODOS` keyed by `(provenance_id, todo_id_hash)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoSnapshot {
    /// Unique todo identifier from the agent framework.
    pub todo_id: String,
    /// Human-readable content / description of the todo.
    pub content: String,
    /// Owner of the todo (agent name or user).
    pub owner: String,
    /// Final status at the end of the turn (e.g. `"done"`, `"in_progress"`).
    pub final_status: String,
    /// Priority level (e.g. `"high"`, `"medium"`, `"low"`).
    pub priority: String,
    /// Source file path, if the todo is anchored to a file.
    pub file: Option<String>,
    /// Start line in the source file (1-based).
    pub start_line: Option<u32>,
    /// End line in the source file (1-based, inclusive).
    pub end_line: Option<u32>,
    /// ISO-8601 timestamp when work on this todo began.
    pub started_at: Option<String>,
    /// ISO-8601 timestamp when this todo was marked complete.
    pub completed_at: Option<String>,
}

/// Timing data for a single phase within a turn.
///
/// Stored in `SESSION_PHASES` keyed by `(provenance_id, phase_hash)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTimingEntry {
    /// Phase name (e.g. `"plan"`, `"implement"`, `"verify"`).
    pub phase: String,
    /// ISO-8601 timestamp when this phase started.
    pub start_ts: Option<String>,
    /// ISO-8601 timestamp when this phase ended.
    pub end_ts: Option<String>,
    /// Number of input (prompt) tokens consumed during this phase.
    pub input_tokens: u64,
    /// Number of output (completion) tokens produced during this phase.
    pub output_tokens: u64,
    /// Estimated cost in USD for this phase.
    pub cost_usd: f64,
}

/// A single event in the Petri net replay log.
///
/// Stored in `SESSION_EVENTS` keyed by `(provenance_id, seq)`.
/// This is the raw event — the other session tables are derived
/// summaries for fast lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonically increasing sequence number within the turn.
    pub seq: u64,
    /// ISO-8601 timestamp of the event.
    pub timestamp: String,
    /// Event kind discriminator (e.g. `"fire"`, `"deposit"`, `"consume"`).
    pub event_kind: String,
    /// Petri net place name, if the event is place-related.
    pub place: Option<String>,
    /// Petri net transition name, if the event is a firing.
    pub transition: Option<String>,
    /// Token identifier within the Petri net.
    pub token_id: String,
    /// Token kind discriminator (e.g. `"intent"`, `"todo"`, `"artifact"`).
    pub token_kind: String,
    /// JSON string of the token's data bag.
    pub token_data: String,
    /// Agent-trace `record_type` for convenience filtering.
    pub record_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Serialization helpers (postcard)
// ---------------------------------------------------------------------------

impl IntentEntry {
    /// Serialize to a compact binary blob via postcard.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("IntentEntry serialization")
    }

    /// Deserialize from a postcard binary blob.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

impl TodoSnapshot {
    /// Serialize to a compact binary blob via postcard.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("TodoSnapshot serialization")
    }

    /// Deserialize from a postcard binary blob.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

impl PhaseTimingEntry {
    /// Serialize to a compact binary blob via postcard.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("PhaseTimingEntry serialization")
    }

    /// Deserialize from a postcard binary blob.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

impl SessionEvent {
    /// Serialize to a compact binary blob via postcard.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SessionEvent serialization")
    }

    /// Deserialize from a postcard binary blob.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// Composite key encoding
// ---------------------------------------------------------------------------

/// Encode a `(provenance_id, seq)` pair as 16 bytes for `SESSION_EVENTS`.
///
/// Layout: `[provenance_id: u64 LE][seq: u64 LE]`
///
/// This follows the same pattern as [`encode_stack_seq`](crate::pristine::tables::encode_stack_seq).
#[inline]
pub fn encode_session_event_key(provenance_id: u64, seq: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_le_bytes());
    key[8..16].copy_from_slice(&seq.to_le_bytes());
    key
}

/// Decode a `(provenance_id, seq)` pair from 16 bytes.
#[inline]
pub fn decode_session_event_key(key: &[u8; 16]) -> (u64, u64) {
    let provenance_id = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let seq = u64::from_le_bytes(key[8..16].try_into().unwrap());
    (provenance_id, seq)
}

/// Encode a `(provenance_id, todo_id_hash)` pair as 16 bytes for `SESSION_TODOS`.
///
/// The `todo_id` string is hashed with Blake3 and the first 8 bytes are used
/// as a fixed-size discriminant.
///
/// Layout: `[provenance_id: u64 LE][blake3(todo_id)[0..8]]`
#[inline]
pub fn encode_session_todo_key(provenance_id: u64, todo_id: &str) -> [u8; 16] {
    let hash = blake3::hash(todo_id.as_bytes());
    let hash_bytes = hash.as_bytes();
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_le_bytes());
    key[8..16].copy_from_slice(&hash_bytes[0..8]);
    key
}

/// Decode the `provenance_id` from a `SESSION_TODOS` key.
///
/// The second 8 bytes are an opaque hash prefix — the original `todo_id`
/// string is stored inside the [`TodoSnapshot`] value.
#[inline]
pub fn decode_session_todo_key_provenance_id(key: &[u8; 16]) -> u64 {
    u64::from_le_bytes(key[0..8].try_into().unwrap())
}

/// Encode a `(provenance_id, phase_hash)` pair as 16 bytes for `SESSION_PHASES`.
///
/// The `phase_name` string is hashed with Blake3 and the first 8 bytes are
/// used as a fixed-size discriminant.
///
/// Layout: `[provenance_id: u64 LE][blake3(phase_name)[0..8]]`
#[inline]
pub fn encode_session_phase_key(provenance_id: u64, phase_name: &str) -> [u8; 16] {
    let hash = blake3::hash(phase_name.as_bytes());
    let hash_bytes = hash.as_bytes();
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_le_bytes());
    key[8..16].copy_from_slice(&hash_bytes[0..8]);
    key
}

/// Decode the `provenance_id` from a `SESSION_PHASES` key.
///
/// The second 8 bytes are an opaque hash prefix — the original phase name
/// is stored inside the [`PhaseTimingEntry`] value.
#[inline]
pub fn decode_session_phase_key_provenance_id(key: &[u8; 16]) -> u64 {
    u64::from_le_bytes(key[0..8].try_into().unwrap())
}

/// Encode a provenance-id prefix for range scans on any session table
/// that uses `[u8; 16]` composite keys.
///
/// Returns a 16-byte key with only `provenance_id` set and the secondary
/// component zeroed. Used as the start bound for prefix scans.
#[inline]
pub fn encode_session_prefix(provenance_id: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_le_bytes());
    key
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_intent_entry() -> IntentEntry {
        IntentEntry {
            title: "Fix authentication bug".into(),
            description: "The login flow fails when the session token expires".into(),
            turn_id: 3,
            model: "claude-sonnet-4-20250514".into(),
            session_id: "sess-abc123".into(),
            outcome: "success".into(),
            total_tokens: 15420,
            total_cost_usd: 0.0462,
        }
    }

    fn sample_todo_snapshot() -> TodoSnapshot {
        TodoSnapshot {
            todo_id: "todo-001".into(),
            content: "Refactor token validation logic".into(),
            owner: "agent".into(),
            final_status: "done".into(),
            priority: "high".into(),
            file: Some("src/auth/token.rs".into()),
            start_line: Some(42),
            end_line: Some(87),
            started_at: Some("2025-01-15T10:30:00Z".into()),
            completed_at: Some("2025-01-15T10:32:15Z".into()),
        }
    }

    fn sample_phase_timing_entry() -> PhaseTimingEntry {
        PhaseTimingEntry {
            phase: "implement".into(),
            start_ts: Some("2025-01-15T10:30:00Z".into()),
            end_ts: Some("2025-01-15T10:32:15Z".into()),
            input_tokens: 8200,
            output_tokens: 3100,
            cost_usd: 0.0281,
        }
    }

    fn sample_session_event() -> SessionEvent {
        SessionEvent {
            seq: 7,
            timestamp: "2025-01-15T10:30:05.123Z".into(),
            event_kind: "fire".into(),
            place: Some("ready".into()),
            transition: Some("plan_to_implement".into()),
            token_id: "tok-42".into(),
            token_kind: "intent".into(),
            token_data: r#"{"goal":"fix auth"}"#.into(),
            record_type: Some("transition_fire".into()),
        }
    }

    // -- Roundtrip tests --

    #[test]
    fn test_intent_entry_roundtrip() {
        let original = sample_intent_entry();
        let bytes = original.to_bytes();
        let recovered = IntentEntry::from_bytes(&bytes).unwrap();

        assert_eq!(original, recovered);
        assert_eq!(recovered.title, "Fix authentication bug");
        assert_eq!(recovered.turn_id, 3);
        assert_eq!(recovered.total_tokens, 15420);
        assert!((recovered.total_cost_usd - 0.0462).abs() < f64::EPSILON);
    }

    #[test]
    fn test_todo_snapshot_roundtrip() {
        let original = sample_todo_snapshot();
        let bytes = original.to_bytes();
        let recovered = TodoSnapshot::from_bytes(&bytes).unwrap();

        assert_eq!(original, recovered);
        assert_eq!(recovered.todo_id, "todo-001");
        assert_eq!(recovered.file, Some("src/auth/token.rs".into()));
        assert_eq!(recovered.start_line, Some(42));
        assert_eq!(recovered.end_line, Some(87));
        assert_eq!(recovered.completed_at, Some("2025-01-15T10:32:15Z".into()));
    }

    #[test]
    fn test_phase_timing_entry_roundtrip() {
        let original = sample_phase_timing_entry();
        let bytes = original.to_bytes();
        let recovered = PhaseTimingEntry::from_bytes(&bytes).unwrap();

        assert_eq!(original, recovered);
        assert_eq!(recovered.phase, "implement");
        assert_eq!(recovered.input_tokens, 8200);
        assert_eq!(recovered.output_tokens, 3100);
        assert!((recovered.cost_usd - 0.0281).abs() < f64::EPSILON);
    }

    #[test]
    fn test_session_event_roundtrip() {
        let original = sample_session_event();
        let bytes = original.to_bytes();
        let recovered = SessionEvent::from_bytes(&bytes).unwrap();

        assert_eq!(original, recovered);
        assert_eq!(recovered.seq, 7);
        assert_eq!(recovered.event_kind, "fire");
        assert_eq!(recovered.place, Some("ready".into()));
        assert_eq!(recovered.transition, Some("plan_to_implement".into()));
        assert_eq!(recovered.record_type, Some("transition_fire".into()));
    }

    // -- Edge cases --

    #[test]
    fn test_intent_entry_with_empty_fields() {
        let entry = IntentEntry {
            title: String::new(),
            description: String::new(),
            turn_id: 0,
            model: String::new(),
            session_id: String::new(),
            outcome: String::new(),
            total_tokens: 0,
            total_cost_usd: 0.0,
        };

        let bytes = entry.to_bytes();
        let recovered = IntentEntry::from_bytes(&bytes).unwrap();

        assert_eq!(entry, recovered);
        assert!(recovered.title.is_empty());
        assert_eq!(recovered.total_tokens, 0);
        assert!((recovered.total_cost_usd).abs() < f64::EPSILON);
    }

    #[test]
    fn test_todo_snapshot_with_no_optional_fields() {
        let snapshot = TodoSnapshot {
            todo_id: "bare".into(),
            content: "minimal".into(),
            owner: "agent".into(),
            final_status: "pending".into(),
            priority: "low".into(),
            file: None,
            start_line: None,
            end_line: None,
            started_at: None,
            completed_at: None,
        };

        let bytes = snapshot.to_bytes();
        let recovered = TodoSnapshot::from_bytes(&bytes).unwrap();

        assert_eq!(snapshot, recovered);
        assert!(recovered.file.is_none());
        assert!(recovered.start_line.is_none());
        assert!(recovered.completed_at.is_none());
    }

    #[test]
    fn test_phase_timing_entry_with_no_timestamps() {
        let entry = PhaseTimingEntry {
            phase: "unknown".into(),
            start_ts: None,
            end_ts: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
        };

        let bytes = entry.to_bytes();
        let recovered = PhaseTimingEntry::from_bytes(&bytes).unwrap();

        assert_eq!(entry, recovered);
        assert!(recovered.start_ts.is_none());
        assert!(recovered.end_ts.is_none());
    }

    #[test]
    fn test_session_event_with_no_optional_fields() {
        let event = SessionEvent {
            seq: 0,
            timestamp: "2025-01-01T00:00:00Z".into(),
            event_kind: "init".into(),
            place: None,
            transition: None,
            token_id: "tok-0".into(),
            token_kind: "system".into(),
            token_data: "{}".into(),
            record_type: None,
        };

        let bytes = event.to_bytes();
        let recovered = SessionEvent::from_bytes(&bytes).unwrap();

        assert_eq!(event, recovered);
        assert!(recovered.place.is_none());
        assert!(recovered.transition.is_none());
        assert!(recovered.record_type.is_none());
    }

    #[test]
    fn test_intent_entry_unicode() {
        let entry = IntentEntry {
            title: "修复认证错误".into(),
            description: "ログインフローの修正".into(),
            turn_id: 1,
            model: "模型-v1".into(),
            session_id: "세션-αβγ".into(),
            outcome: "réussite".into(),
            total_tokens: 999,
            total_cost_usd: 1.23,
        };

        let bytes = entry.to_bytes();
        let recovered = IntentEntry::from_bytes(&bytes).unwrap();

        assert_eq!(entry, recovered);
    }

    #[test]
    fn test_invalid_bytes_returns_error() {
        let garbage = &[0xFF, 0xFE, 0xFD, 0xFC, 0xFB];

        assert!(IntentEntry::from_bytes(garbage).is_err());
        assert!(TodoSnapshot::from_bytes(garbage).is_err());
        assert!(PhaseTimingEntry::from_bytes(garbage).is_err());
        assert!(SessionEvent::from_bytes(garbage).is_err());
    }

    #[test]
    fn test_empty_bytes_returns_error() {
        assert!(IntentEntry::from_bytes(&[]).is_err());
        assert!(TodoSnapshot::from_bytes(&[]).is_err());
        assert!(PhaseTimingEntry::from_bytes(&[]).is_err());
        assert!(SessionEvent::from_bytes(&[]).is_err());
    }

    // -- Composite key encoding tests --

    #[test]
    fn test_session_event_key_roundtrip() {
        let prov_id = 42u64;
        let seq = 1337u64;

        let key = encode_session_event_key(prov_id, seq);
        let (dec_prov, dec_seq) = decode_session_event_key(&key);

        assert_eq!(dec_prov, prov_id);
        assert_eq!(dec_seq, seq);
    }

    #[test]
    fn test_session_event_key_ordering() {
        // Keys for the same provenance_id should sort by seq
        let k1 = encode_session_event_key(5, 0);
        let k2 = encode_session_event_key(5, 1);
        let k3 = encode_session_event_key(5, 100);
        let k4 = encode_session_event_key(6, 0);

        assert!(k1 < k2);
        assert!(k2 < k3);
        assert!(k3 < k4, "different provenance_id sorts after");
    }

    #[test]
    fn test_session_todo_key_deterministic() {
        let k1 = encode_session_todo_key(10, "todo-abc");
        let k2 = encode_session_todo_key(10, "todo-abc");

        assert_eq!(k1, k2, "same inputs produce same key");
    }

    #[test]
    fn test_session_todo_key_different_ids() {
        let k1 = encode_session_todo_key(10, "todo-abc");
        let k2 = encode_session_todo_key(10, "todo-xyz");

        // Same provenance_id, different todo_id → different key
        assert_ne!(k1, k2);
        // First 8 bytes (provenance_id) should be the same
        assert_eq!(k1[0..8], k2[0..8]);
    }

    #[test]
    fn test_session_todo_key_provenance_id_decode() {
        let key = encode_session_todo_key(77, "some-todo");
        assert_eq!(decode_session_todo_key_provenance_id(&key), 77);
    }

    #[test]
    fn test_session_phase_key_deterministic() {
        let k1 = encode_session_phase_key(10, "plan");
        let k2 = encode_session_phase_key(10, "plan");

        assert_eq!(k1, k2);
    }

    #[test]
    fn test_session_phase_key_different_phases() {
        let k1 = encode_session_phase_key(10, "plan");
        let k2 = encode_session_phase_key(10, "implement");

        assert_ne!(k1, k2);
        assert_eq!(k1[0..8], k2[0..8]);
    }

    #[test]
    fn test_session_phase_key_provenance_id_decode() {
        let key = encode_session_phase_key(88, "verify");
        assert_eq!(decode_session_phase_key_provenance_id(&key), 88);
    }

    #[test]
    fn test_session_prefix() {
        let prefix = encode_session_prefix(42);
        let (prov_id, secondary) = decode_session_event_key(&prefix);

        assert_eq!(prov_id, 42);
        assert_eq!(secondary, 0);
    }

    #[test]
    fn test_session_prefix_ordering() {
        // All keys for provenance_id=5 should be >= prefix(5) and < prefix(6)
        let prefix_5 = encode_session_prefix(5);
        let key_5_0 = encode_session_event_key(5, 0);
        let key_5_max = encode_session_event_key(5, u64::MAX);
        let prefix_6 = encode_session_prefix(6);

        assert_eq!(prefix_5, key_5_0);
        assert!(key_5_max < prefix_6);
    }

    #[test]
    fn test_session_event_key_boundary_values() {
        // Min values
        let key_min = encode_session_event_key(0, 0);
        let (p, s) = decode_session_event_key(&key_min);
        assert_eq!(p, 0);
        assert_eq!(s, 0);

        // Max values
        let key_max = encode_session_event_key(u64::MAX, u64::MAX);
        let (p, s) = decode_session_event_key(&key_max);
        assert_eq!(p, u64::MAX);
        assert_eq!(s, u64::MAX);
    }

    #[test]
    fn test_postcard_is_compact() {
        // Verify that postcard produces reasonably compact output.
        // An IntentEntry with short strings should fit in well under 200 bytes.
        let entry = sample_intent_entry();
        let bytes = entry.to_bytes();

        assert!(
            bytes.len() < 200,
            "expected compact serialization, got {} bytes",
            bytes.len()
        );
    }
}

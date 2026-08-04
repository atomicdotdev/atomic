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
//! Composite keys are 16-byte arrays where:
//! first 8 bytes = discriminant (`u64` BE, for correct numeric ordering in
//! lexicographic range scans), second 8 bytes = secondary key (`u64` BE or
//! first 8 bytes of a BLAKE3 hash).

use serde::{Deserialize, Serialize};

use crate::types::Hash;

/// Repository-indexed identity and current state for an agent session.
///
/// The JSON file under `.atomic/sessions/` remains mutable runtime state. This
/// record is the durable Atomic index that connects that file to the immutable
/// turn/provenance ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub json_path: String,
    pub view_name: Option<String>,
    pub parent_view: Option<String>,
    pub first_provenance: Option<Hash>,
    pub latest_provenance: Option<Hash>,
    pub turn_count: u32,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// Index entry connecting one session turn to its immutable Atomic objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub session_id: String,
    pub turn_number: u32,
    pub goal: Option<String>,
    pub provenance_hash: Hash,
    pub change_hashes: Vec<Hash>,
    pub previous_provenance: Option<Hash>,
    pub timestamp: i64,
    /// Stable vault work-item/intent ID governing this turn, when known.
    #[serde(default)]
    pub plan_id: Option<String>,
    /// Todo snapshot captured at the end of this turn.
    #[serde(default)]
    pub todos: Vec<SessionTodo>,
}

/// Return a deterministic, causality-aware ordering for a session's turns.
///
/// Stored provenance can arrive in any order (for example during pull). A
/// timestamp plus full provenance hash provides a stable tie-break, while the
/// `previous_provenance` link keeps a child after its parent when both are
/// present. Turn numbers are reassigned from zero after ordering.
pub fn canonicalize_session_turns(turns: Vec<SessionTurn>) -> Vec<SessionTurn> {
    type SortKey = (i64, [u8; 32], usize);

    let count = turns.len();
    let positions: std::collections::HashMap<Hash, usize> = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.provenance_hash, index))
        .collect();
    let sort_keys: Vec<SortKey> = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.timestamp, *turn.provenance_hash.as_bytes(), index))
        .collect();
    let mut children = vec![Vec::new(); count];
    let mut blocked = vec![false; count];

    for (index, turn) in turns.iter().enumerate() {
        if let Some(parent) = turn
            .previous_provenance
            .and_then(|hash| positions.get(&hash).copied())
        {
            blocked[index] = true;
            children[parent].push(index);
        }
    }

    let mut remaining: std::collections::BTreeSet<SortKey> = sort_keys.iter().copied().collect();
    let mut ready: std::collections::BTreeSet<SortKey> = sort_keys
        .iter()
        .copied()
        .filter(|key| !blocked[key.2])
        .collect();
    let mut slots: Vec<Option<SessionTurn>> = turns.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(count);

    while ordered.len() < count {
        // If malformed provenance contains a cycle, break it at the same
        // timestamp/hash point in every repository.
        let key = ready
            .pop_first()
            .or_else(|| remaining.first().copied())
            .expect("remaining turn while canonicalizing");
        remaining.remove(&key);
        let index = key.2;
        let Some(turn) = slots[index].take() else {
            continue;
        };
        ordered.push(turn);

        for child in &children[index] {
            if blocked[*child] {
                blocked[*child] = false;
                ready.insert(sort_keys[*child]);
            }
        }
    }

    for (turn_number, turn) in ordered.iter_mut().enumerate() {
        turn.turn_number = turn_number as u32;
    }
    ordered
}

/// Generic todo snapshot carried by provenance and the session ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTodo {
    /// Upstream stable ID when supplied; otherwise a turn-local snapshot ID.
    pub id: String,
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// Pre-plan/todo SessionTurn encoding (ATOM-15).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTurnV1 {
    session_id: String,
    turn_number: u32,
    goal: Option<String>,
    provenance_hash: Hash,
    change_hashes: Vec<Hash>,
    previous_provenance: Option<Hash>,
    timestamp: i64,
}

impl From<SessionTurnV1> for SessionTurn {
    fn from(value: SessionTurnV1) -> Self {
        Self {
            session_id: value.session_id,
            turn_number: value.turn_number,
            goal: value.goal,
            provenance_hash: value.provenance_hash,
            change_hashes: value.change_hashes,
            previous_provenance: value.previous_provenance,
            timestamp: value.timestamp,
            plan_id: None,
            todos: Vec::new(),
        }
    }
}

/// Pre-plan/todo SessionManifest encoding (schema v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionManifestV1 {
    schema_version: u8,
    session_id: String,
    goal_provenance: Option<Hash>,
    turns: Vec<SessionTurnV1>,
    parent_session: Option<Hash>,
    fork_turn: Option<u32>,
}

/// Portable, content-addressed root for an Atomic session ledger.
///
/// The manifest deliberately contains external hashes and no repository-local
/// Redb IDs. Local indexes can be rebuilt from it after synchronization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u8,
    pub session_id: String,
    pub goal_provenance: Option<Hash>,
    pub turns: Vec<SessionTurn>,
    pub parent_session: Option<Hash>,
    pub fork_turn: Option<u32>,
}

impl SessionRecord {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SessionRecord serialization")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

impl SessionTurn {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SessionTurn serialization")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
            .or_else(|_| postcard::from_bytes::<SessionTurnV1>(bytes).map(SessionTurn::from))
    }
}

impl SessionManifest {
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("SessionManifest serialization")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes).or_else(|_| {
            postcard::from_bytes::<SessionManifestV1>(bytes).map(|legacy| Self {
                schema_version: 2,
                session_id: legacy.session_id,
                goal_provenance: legacy.goal_provenance,
                turns: legacy.turns.into_iter().map(SessionTurn::from).collect(),
                parent_session: legacy.parent_session,
                fork_turn: legacy.fork_turn,
            })
        })
    }

    /// Return the content hash of this exact manifest encoding.
    pub fn content_hash(&self) -> Hash {
        Hash::of(&self.to_bytes())
    }
}

/// Encode `(session_namespace, turn_number)` for ordered Redb scans.
#[inline]
pub fn encode_session_turn_key(session_id: [u8; 32], turn_number: u32) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[0..32].copy_from_slice(&session_id);
    key[36..40].copy_from_slice(&turn_number.to_be_bytes());
    key
}

/// Decode a session turn key.
#[inline]
pub fn decode_session_turn_key(key: &[u8; 40]) -> ([u8; 32], u32) {
    let session_id = key[0..32].try_into().unwrap();
    let turn_number = u32::from_be_bytes(key[36..40].try_into().unwrap());
    (session_id, turn_number)
}

/// Derive the local ordered-index prefix for an external session ID.
#[inline]
pub fn session_turn_namespace(session_id: &str) -> [u8; 32] {
    *blake3::hash(session_id.as_bytes()).as_bytes()
}

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
/// Layout: `[provenance_id: u64 BE][seq: u64 BE]`
///
/// Big-endian encoding is required so that lexicographic byte order (used by
/// the B-tree) matches numeric order, enabling correct prefix range scans.
#[inline]
pub fn encode_session_event_key(provenance_id: u64, seq: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_be_bytes());
    key[8..16].copy_from_slice(&seq.to_be_bytes());
    key
}

/// Decode a `(provenance_id, seq)` pair from 16 bytes.
#[inline]
pub fn decode_session_event_key(key: &[u8; 16]) -> (u64, u64) {
    let provenance_id = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let seq = u64::from_be_bytes(key[8..16].try_into().unwrap());
    (provenance_id, seq)
}

/// Encode a `(provenance_id, todo_id_hash)` pair as 16 bytes for `SESSION_TODOS`.
///
/// The `todo_id` string is hashed with Blake3 and the first 8 bytes are used
/// as a fixed-size discriminant.
///
/// Layout: `[provenance_id: u64 BE][blake3(todo_id)[0..8]]`
///
/// Big-endian encoding is required so that lexicographic byte order (used by
/// the B-tree) matches numeric order, enabling correct prefix range scans.
#[inline]
pub fn encode_session_todo_key(provenance_id: u64, todo_id: &str) -> [u8; 16] {
    let hash = blake3::hash(todo_id.as_bytes());
    let hash_bytes = hash.as_bytes();
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_be_bytes());
    key[8..16].copy_from_slice(&hash_bytes[0..8]);
    key
}

/// Decode the `provenance_id` from a `SESSION_TODOS` key.
///
/// The second 8 bytes are an opaque hash prefix — the original `todo_id`
/// string is stored inside the [`TodoSnapshot`] value.
#[inline]
pub fn decode_session_todo_key_provenance_id(key: &[u8; 16]) -> u64 {
    u64::from_be_bytes(key[0..8].try_into().unwrap())
}

/// Encode a `(provenance_id, phase_hash)` pair as 16 bytes for `SESSION_PHASES`.
///
/// The `phase_name` string is hashed with Blake3 and the first 8 bytes are
/// used as a fixed-size discriminant.
///
/// Layout: `[provenance_id: u64 BE][blake3(phase_name)[0..8]]`
///
/// Big-endian encoding is required so that lexicographic byte order (used by
/// the B-tree) matches numeric order, enabling correct prefix range scans.
#[inline]
pub fn encode_session_phase_key(provenance_id: u64, phase_name: &str) -> [u8; 16] {
    let hash = blake3::hash(phase_name.as_bytes());
    let hash_bytes = hash.as_bytes();
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_be_bytes());
    key[8..16].copy_from_slice(&hash_bytes[0..8]);
    key
}

/// Decode the `provenance_id` from a `SESSION_PHASES` key.
///
/// The second 8 bytes are an opaque hash prefix — the original phase name
/// is stored inside the [`PhaseTimingEntry`] value.
#[inline]
pub fn decode_session_phase_key_provenance_id(key: &[u8; 16]) -> u64 {
    u64::from_be_bytes(key[0..8].try_into().unwrap())
}

/// Encode a provenance-id prefix for range scans on any session table
/// that uses `[u8; 16]` composite keys.
///
/// Returns a 16-byte key with `provenance_id` in the high bytes and the
/// secondary component zeroed. Used as the start bound for prefix scans:
/// all keys for a given `provenance_id` lie in `[prefix(id), prefix(id+1))`.
///
/// Big-endian encoding ensures this half-open range is correct across all
/// numeric values of `provenance_id`.
#[inline]
pub fn encode_session_prefix(provenance_id: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&provenance_id.to_be_bytes());
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
        // Keys for the same provenance_id must sort by seq in lexicographic
        // (B-tree) order across the full u64 range.  Big-endian encoding
        // guarantees this: the most-significant byte comes first, so byte-wise
        // comparison matches numeric comparison for all values.
        let k1 = encode_session_event_key(5, 0);
        let k2 = encode_session_event_key(5, 1);
        let k3 = encode_session_event_key(5, 100);
        let k4 = encode_session_event_key(6, 0);

        assert!(k1 < k2);
        assert!(k2 < k3);
        assert!(k3 < k4, "different provenance_id sorts after");

        // Boundary: seq=255 (single-byte max) vs seq=256 (spills into second byte).
        // With big-endian encoding this must sort correctly.
        let k_255 = encode_session_event_key(5, 255);
        let k_256 = encode_session_event_key(5, 256);
        assert!(
            k_255 < k_256,
            "seq=255 must sort before seq=256 (byte-spill boundary)"
        );

        // seq=256 on provenance_id=5 must still sort before provenance_id=6.
        assert!(
            k_256 < k4,
            "seq=256 on provenance_id=5 must sort before provenance_id=6"
        );
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

    #[test]
    fn test_session_record_roundtrip() {
        let record = SessionRecord {
            session_id: "sess-1".into(),
            json_path: ".atomic/sessions/sess-1.json".into(),
            view_name: Some("agent-sess-1".into()),
            parent_view: Some("main".into()),
            first_provenance: Some(Hash::of(b"p0")),
            latest_provenance: Some(Hash::of(b"p1")),
            turn_count: 2,
            started_at: 100,
            ended_at: None,
        };

        assert_eq!(
            SessionRecord::from_bytes(&record.to_bytes()).unwrap(),
            record
        );
    }

    #[test]
    fn test_session_turn_roundtrip_and_key_ordering() {
        let turn = SessionTurn {
            session_id: "sess-1".into(),
            turn_number: 7,
            goal: Some("Test goal".into()),
            provenance_hash: Hash::of(b"p7"),
            change_hashes: vec![Hash::of(b"c7")],
            previous_provenance: Some(Hash::of(b"p6")),
            timestamp: 700,
            plan_id: Some("ATOM-20".into()),
            todos: vec![SessionTodo {
                id: "todo-1".into(),
                content: "Wire RDF links".into(),
                status: "completed".into(),
                priority: "high".into(),
            }],
        };
        assert_eq!(SessionTurn::from_bytes(&turn.to_bytes()).unwrap(), turn);

        let namespace = session_turn_namespace("sess-1");
        let earlier = encode_session_turn_key(namespace, 1);
        let later = encode_session_turn_key(namespace, 2);
        assert!(earlier < later);
        assert_eq!(decode_session_turn_key(&later), (namespace, 2));
    }

    #[test]
    fn test_session_turn_order_is_independent_of_ingestion_order() {
        let parent_hash = Hash::of(b"parent");
        let child_hash = Hash::of(b"child");
        let make_turns = || {
            vec![
                SessionTurn {
                    session_id: "sess-1".into(),
                    turn_number: 0,
                    goal: None,
                    provenance_hash: parent_hash,
                    change_hashes: Vec::new(),
                    previous_provenance: None,
                    timestamp: 100,
                    plan_id: None,
                    todos: Vec::new(),
                },
                SessionTurn {
                    session_id: "sess-1".into(),
                    turn_number: 1,
                    goal: None,
                    provenance_hash: child_hash,
                    change_hashes: Vec::new(),
                    previous_provenance: Some(parent_hash),
                    // Clock skew must not place a child before its parent.
                    timestamp: 50,
                    plan_id: None,
                    todos: Vec::new(),
                },
            ]
        };

        let forward = canonicalize_session_turns(make_turns());
        let mut reverse_input = make_turns();
        reverse_input.reverse();
        let reverse = canonicalize_session_turns(reverse_input);

        assert_eq!(forward, reverse);
        assert_eq!(forward[0].provenance_hash, parent_hash);
        assert_eq!(forward[1].previous_provenance, Some(parent_hash));
        assert_eq!(
            forward
                .iter()
                .map(|turn| turn.turn_number)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn test_session_turn_order_uses_hash_to_break_timestamp_ties() {
        let make_turn = |hash: Hash| SessionTurn {
            session_id: "sess-1".into(),
            turn_number: 99,
            goal: None,
            provenance_hash: hash,
            change_hashes: Vec::new(),
            previous_provenance: None,
            timestamp: 100,
            plan_id: None,
            todos: Vec::new(),
        };
        let a = make_turn(Hash::of(b"a"));
        let b = make_turn(Hash::of(b"b"));

        let forward = canonicalize_session_turns(vec![a.clone(), b.clone()]);
        let reverse = canonicalize_session_turns(vec![b, a]);

        assert_eq!(forward, reverse);
        assert!(forward[0].provenance_hash.as_bytes() < forward[1].provenance_hash.as_bytes());
    }

    #[test]
    fn test_session_turn_reads_pre_context_encoding() {
        #[derive(Serialize)]
        struct LegacyTurn {
            session_id: String,
            turn_number: u32,
            goal: Option<String>,
            provenance_hash: Hash,
            change_hashes: Vec<Hash>,
            previous_provenance: Option<Hash>,
            timestamp: i64,
        }

        let legacy = LegacyTurn {
            session_id: "legacy".into(),
            turn_number: 0,
            goal: Some("Old turn".into()),
            provenance_hash: Hash::of(b"p"),
            change_hashes: vec![Hash::of(b"c")],
            previous_provenance: None,
            timestamp: 1,
        };
        let bytes = postcard::to_allocvec(&legacy).unwrap();
        let loaded = SessionTurn::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.session_id, "legacy");
        assert_eq!(loaded.plan_id, None);
        assert!(loaded.todos.is_empty());
    }

    #[test]
    fn test_session_manifest_roundtrip_and_content_hash() {
        let manifest = SessionManifest {
            schema_version: 2,
            session_id: "sess-1".into(),
            goal_provenance: Some(Hash::of(b"goal")),
            turns: Vec::new(),
            parent_session: None,
            fork_turn: None,
        };
        let bytes = manifest.to_bytes();
        assert_eq!(SessionManifest::from_bytes(&bytes).unwrap(), manifest);
        assert_eq!(manifest.content_hash(), Hash::of(&bytes));
    }

    #[test]
    fn test_session_manifest_reads_schema_v1_turns() {
        let legacy = SessionManifestV1 {
            schema_version: 1,
            session_id: "legacy-manifest".into(),
            goal_provenance: None,
            turns: vec![SessionTurnV1 {
                session_id: "legacy-manifest".into(),
                turn_number: 0,
                goal: Some("Old".into()),
                provenance_hash: Hash::of(b"p"),
                change_hashes: vec![],
                previous_provenance: None,
                timestamp: 1,
            }],
            parent_session: None,
            fork_turn: None,
        };
        let bytes = postcard::to_allocvec(&legacy).unwrap();
        let loaded = SessionManifest::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.schema_version, 2);
        assert_eq!(loaded.turns.len(), 1);
        assert_eq!(loaded.turns[0].plan_id, None);
        assert!(loaded.turns[0].todos.is_empty());
    }
}

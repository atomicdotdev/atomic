//! Session envelope for embedding turn/session data inside Atomic changes.
//!
//! The `SessionEnvelope` is the structured type that gets serialized into
//! `HashedChange.metadata` (the `Vec<u8>` field). Because this field is
//! **hashed**, the session envelope becomes part of the change's cryptographic
//! identity — making it tamper-evident.
//!
//! # Why This Exists
//!
//! Atomic changes have three data slots:
//!
//! | Slot | Hashed? | Session Data |
//! |---|---|---|
//! | `hashed.provenance` | Yes | Per-turn metrics (tokens, cost, model) |
//! | **`hashed.metadata`** | **Yes** | **Session envelope (this module)** |
//! | `unhashed` | No | Transcript (large, redactable) |
//!
//! The session envelope carries the **structural metadata** that links turns
//! into sessions: session ID, turn number, timing, files touched, agent name.
//! This data is small (~200-500 bytes) and must be tamper-evident, so it
//! belongs in the hashed section.
//!
//! # Wire Format
//!
//! The envelope is serialized as:
//!
//! ```text
//! [magic: 4 bytes "ATSE"] [postcard payload]
//! ```
//!
//! The 4-byte magic prefix allows `is_session_envelope()` to quickly check
//! whether a change's metadata field contains a session envelope vs other
//! metadata uses. The schema version allows forward-compatible evolution.
//!
//! # Commutation
//!
//! Because the envelope is inside the change, it **commutes** via patch theory
//! exactly like any other change data. When changes are pushed to a remote,
//! the session data arrives automatically. The server can reconstruct full
//! session timelines by scanning change metadata — no separate sync protocol,
//! no metadata branch, no side-channel.
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::envelope::SessionEnvelope;
//!
//! let envelope = SessionEnvelope::builder("session-abc", "claude-code")
//!     .turn_number(3)
//!     .agent_display_name("Claude Code")
//!     .prompt_summary("Fix the authentication bug in login.rs")
//!     .files_in_turn(vec!["src/auth.rs".to_string(), "src/auth_test.rs".to_string()])
//!     .turn_duration_ms(12400)
//!     .build();
//!
//! // Encode for storage in HashedChange.metadata
//! let bytes = envelope.encode().unwrap();
//! assert!(SessionEnvelope::is_session_envelope(&bytes));
//!
//! // Decode back
//! let decoded = SessionEnvelope::decode(&bytes).unwrap();
//! assert_eq!(decoded.session_id, "session-abc");
//! assert_eq!(decoded.turn_number, 3);
//! ```

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};

// =============================================================================
// Constants
// =============================================================================

/// Magic bytes identifying a SessionEnvelope in HashedChange.metadata.
///
/// "ATSE" = Atomic Turn Session Envelope.
const MAGIC: &[u8; 4] = b"ATSE";

/// Current schema version. Increment when making breaking changes to the
/// envelope format.
const SCHEMA_VERSION: u8 = 1;

/// Minimum valid encoded size: magic (4) + at least 1 byte of postcard payload.
#[allow(dead_code)]
const MIN_ENCODED_SIZE: usize = 5;

// =============================================================================
// SessionEnvelope
// =============================================================================

/// Structured session/turn metadata embedded in `HashedChange.metadata`.
///
/// This type is serialized (magic + postcard) into the hashed
/// metadata field of an Atomic change, making it part of the change's
/// cryptographic identity.
///
/// # Fields
///
/// The envelope carries two categories of data:
///
/// **Turn-specific** (unique to this change):
/// - `turn_number` — sequential within the session
/// - `turn_started_at` / `turn_ended_at` — wall clock timestamps
/// - `turn_duration_ms` — how long the agent worked
/// - `prompt_summary` — first ~200 chars of the user's prompt
/// - `prompt_hash` — Blake3 hash of the full prompt
/// - `files_in_turn` — files modified in THIS turn
///
/// **Session-level** (shared across turns in the same session):
/// - `session_id` — links turns together
/// - `agent_name` — "claude-code", "gemini-cli", etc.
/// - `agent_display_name` — "Claude Code", "Gemini CLI"
/// - `session_started_at` — when the session began
/// - `total_turns` — backfilled on session end
/// - `files_in_session` — cumulative unique file count
/// - `delegation_id` — identity delegation reference
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnvelope {
    /// Schema version for forward compatibility.
    ///
    /// Readers should reject envelopes with a version they don't understand.
    /// Writers always use `SCHEMA_VERSION`.
    pub schema_version: u8,

    // =========================================================================
    // Session identification
    // =========================================================================
    /// Unique session identifier.
    ///
    /// Links all turns within the same session. Format is agent-specific
    /// (e.g., UUID for Claude Code, path-derived for Gemini CLI).
    pub session_id: String,

    /// Agent registry key (e.g., "claude-code", "gemini-cli", "codex").
    ///
    /// Used for programmatic lookup and agent-specific behavior.
    pub agent_name: String,

    /// Human-readable agent name (e.g., "Claude Code", "Gemini CLI").
    ///
    /// Used for display in UI and log messages.
    #[serde(default)]
    pub agent_display_name: Option<String>,

    // =========================================================================
    // Turn identification
    // =========================================================================
    /// Sequential turn number within the session (1-indexed).
    ///
    /// Turn 1 is the first turn after the session starts. The server uses
    /// this to order turns within a session timeline.
    pub turn_number: u32,

    /// Total number of turns in the session.
    ///
    /// `None` during the session (not yet known). Backfilled on session end
    /// by updating the last turn's envelope. The server uses this to show
    /// "turn 3 of 7" in the UI.
    #[serde(default)]
    pub total_turns: Option<u32>,

    // =========================================================================
    // Timing
    // =========================================================================
    /// When the session started (Unix epoch seconds).
    pub session_started_at: i64,

    /// When this turn started (Unix epoch seconds).
    pub turn_started_at: i64,

    /// When this turn ended (Unix epoch seconds).
    pub turn_ended_at: i64,

    /// Wall clock duration of this turn in milliseconds.
    ///
    /// This is `turn_ended_at - turn_started_at` in ms, stored explicitly
    /// to avoid timezone/precision issues in the UI.
    pub turn_duration_ms: u64,

    // =========================================================================
    // Prompt
    // =========================================================================
    /// First ~200 characters of the user's prompt for UI previews.
    ///
    /// Truncated at word boundaries when possible. `None` if the agent
    /// didn't provide the prompt (some hooks don't include it).
    #[serde(default)]
    pub prompt_summary: Option<String>,

    /// Blake3 hash of the full prompt text (32 bytes).
    ///
    /// Used for deduplication detection (same prompt submitted multiple times)
    /// and for linking to the full prompt in the transcript (unhashed section).
    /// `None` if no prompt was available.
    #[serde(default)]
    pub prompt_hash: Option<[u8; 32]>,

    // =========================================================================
    // Files
    // =========================================================================
    /// Files modified in THIS turn.
    ///
    /// Paths are relative to the repository root. This is the same list
    /// passed to `RecordOptions::paths` when creating the Atomic change.
    #[serde(default)]
    pub files_in_turn: Vec<String>,

    /// Cumulative count of unique files touched across the entire session.
    ///
    /// Updated on each turn. The server uses this for session-level metrics
    /// without needing to scan all turns.
    #[serde(default)]
    pub files_in_session: u32,

    // =========================================================================
    // Identity
    // =========================================================================
    /// Reference to the identity delegation authorizing this agent.
    ///
    /// This is the delegation ID from `atomic-identity`, linking the change
    /// to the user who authorized the agent to act on their behalf.
    /// `None` if no delegation is configured (unsigned agent changes).
    #[serde(default)]
    pub delegation_id: Option<String>,
}

impl SessionEnvelope {
    /// Create a new `SessionEnvelopeBuilder` with the required fields.
    ///
    /// # Arguments
    ///
    /// * `session_id` — Unique session identifier
    /// * `agent_name` — Agent registry key (e.g., "claude-code")
    pub fn builder(
        session_id: impl Into<String>,
        agent_name: impl Into<String>,
    ) -> SessionEnvelopeBuilder {
        SessionEnvelopeBuilder::new(session_id, agent_name)
    }

    /// Encode the envelope for storage in `HashedChange.metadata`.
    ///
    /// Format: `[MAGIC: 4 bytes][postcard payload]`
    ///
    /// # Errors
    ///
    /// Returns `AgentError::EnvelopeCodecError` if postcard serialization fails.
    pub fn encode(&self) -> AgentResult<Vec<u8>> {
        let payload = postcard::to_allocvec(self).map_err(|e| AgentError::EnvelopeCodecError {
            reason: format!("postcard serialize failed: {}", e),
        })?;

        let mut buf = Vec::with_capacity(MAGIC.len() + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Decode a `SessionEnvelope` from `HashedChange.metadata` bytes.
    ///
    /// Validates the magic prefix and schema version before deserializing.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::EnvelopeCodecError` if:
    /// - The data is too short
    /// - The magic prefix doesn't match
    /// - The schema version is unsupported
    /// - Postcard deserialization fails
    pub fn decode(data: &[u8]) -> AgentResult<Self> {
        if data.len() < MAGIC.len() + 1 {
            return Err(AgentError::EnvelopeCodecError {
                reason: format!(
                    "data too short: {} bytes (minimum {})",
                    data.len(),
                    MAGIC.len() + 1
                ),
            });
        }

        // Check magic
        if &data[..4] != MAGIC {
            return Err(AgentError::EnvelopeCodecError {
                reason: format!("invalid magic: expected {:?}, got {:?}", MAGIC, &data[..4]),
            });
        }

        // Deserialize payload (includes schema_version field)
        let envelope: Self =
            postcard::from_bytes(&data[4..]).map_err(|e| AgentError::EnvelopeCodecError {
                reason: format!("postcard deserialize failed: {}", e),
            })?;

        // Check version from the deserialized struct
        if envelope.schema_version > SCHEMA_VERSION {
            return Err(AgentError::EnvelopeCodecError {
                reason: format!(
                    "unsupported schema version: {} (max supported: {})",
                    envelope.schema_version, SCHEMA_VERSION
                ),
            });
        }

        Ok(envelope)
    }

    /// Check whether a byte slice looks like a `SessionEnvelope`.
    ///
    /// This is a fast check (just the 4-byte magic prefix) that the server
    /// can use to identify which changes have session data without fully
    /// deserializing. Used during push receipt to index session turns.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::envelope::SessionEnvelope;
    ///
    /// let envelope = SessionEnvelope::builder("s1", "claude-code").build();
    /// let bytes = envelope.encode().unwrap();
    ///
    /// assert!(SessionEnvelope::is_session_envelope(&bytes));
    /// assert!(!SessionEnvelope::is_session_envelope(b"not an envelope"));
    /// assert!(!SessionEnvelope::is_session_envelope(b""));
    /// assert!(!SessionEnvelope::is_session_envelope(b"ATS")); // too short
    /// ```
    pub fn is_session_envelope(data: &[u8]) -> bool {
        data.len() >= MAGIC.len() && &data[..4] == MAGIC
    }

    /// Returns the number of files changed in this turn.
    pub fn turn_file_count(&self) -> usize {
        self.files_in_turn.len()
    }

    /// Returns `true` if a prompt summary is available.
    pub fn has_prompt(&self) -> bool {
        self.prompt_summary.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// Returns `true` if total_turns has been backfilled (session is complete).
    pub fn is_session_complete(&self) -> bool {
        self.total_turns.is_some()
    }

    /// Returns the turn duration as a human-readable string.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::envelope::SessionEnvelope;
    ///
    /// let e = SessionEnvelope::builder("s", "a")
    ///     .turn_duration_ms(125_400)
    ///     .build();
    /// assert_eq!(e.duration_display(), "2m 5s");
    ///
    /// let e = SessionEnvelope::builder("s", "a")
    ///     .turn_duration_ms(800)
    ///     .build();
    /// assert_eq!(e.duration_display(), "800ms");
    /// ```
    pub fn duration_display(&self) -> String {
        let ms = self.turn_duration_ms;
        if ms < 1_000 {
            format!("{}ms", ms)
        } else if ms < 60_000 {
            let secs = ms as f64 / 1_000.0;
            if secs == secs.floor() {
                format!("{}s", secs as u64)
            } else {
                format!("{:.1}s", secs)
            }
        } else {
            let minutes = ms / 60_000;
            let seconds = (ms % 60_000) / 1_000;
            if seconds == 0 {
                format!("{}m", minutes)
            } else {
                format!("{}m {}s", minutes, seconds)
            }
        }
    }
}

impl std::fmt::Display for SessionEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Turn {} of session {} ({}, {})",
            self.turn_number,
            self.session_id,
            self.agent_name,
            self.duration_display(),
        )?;
        if let Some(ref prompt) = self.prompt_summary {
            let display = if prompt.len() > 60 {
                let truncated: String = prompt.chars().take(57).collect();
                format!("{}...", truncated)
            } else {
                prompt.clone()
            };
            write!(f, " — \"{}\"", display)?;
        }
        Ok(())
    }
}

// =============================================================================
// SessionEnvelopeBuilder
// =============================================================================

/// Builder for constructing `SessionEnvelope` instances.
///
/// Provides a fluent API for setting optional fields. The required fields
/// (`session_id`, `agent_name`) are set in the constructor.
///
/// # Example
///
/// ```rust
/// use atomic_agent::envelope::SessionEnvelope;
///
/// let envelope = SessionEnvelope::builder("session-abc", "claude-code")
///     .agent_display_name("Claude Code")
///     .turn_number(1)
///     .session_started_at(1737000000)
///     .turn_started_at(1737000100)
///     .turn_ended_at(1737000112)
///     .turn_duration_ms(12000)
///     .prompt_summary("Fix the bug")
///     .files_in_turn(vec!["src/main.rs".to_string()])
///     .files_in_session(1)
///     .build();
///
/// assert_eq!(envelope.session_id, "session-abc");
/// assert_eq!(envelope.turn_number, 1);
/// ```
#[derive(Debug)]
pub struct SessionEnvelopeBuilder {
    session_id: String,
    agent_name: String,
    agent_display_name: Option<String>,
    turn_number: u32,
    total_turns: Option<u32>,
    session_started_at: i64,
    turn_started_at: i64,
    turn_ended_at: i64,
    turn_duration_ms: u64,
    prompt_summary: Option<String>,
    prompt_hash: Option<[u8; 32]>,
    files_in_turn: Vec<String>,
    files_in_session: u32,
    delegation_id: Option<String>,
}

impl SessionEnvelopeBuilder {
    /// Create a new builder with required fields.
    pub fn new(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_name: agent_name.into(),
            agent_display_name: None,
            turn_number: 1,
            total_turns: None,
            session_started_at: 0,
            turn_started_at: 0,
            turn_ended_at: 0,
            turn_duration_ms: 0,
            prompt_summary: None,
            prompt_hash: None,
            files_in_turn: Vec::new(),
            files_in_session: 0,
            delegation_id: None,
        }
    }

    /// Set the human-readable agent display name.
    #[must_use]
    pub fn agent_display_name(mut self, name: impl Into<String>) -> Self {
        self.agent_display_name = Some(name.into());
        self
    }

    /// Set the turn number (1-indexed).
    #[must_use]
    pub fn turn_number(mut self, n: u32) -> Self {
        self.turn_number = n;
        self
    }

    /// Set the total number of turns in the session (backfill on session end).
    #[must_use]
    pub fn total_turns(mut self, n: u32) -> Self {
        self.total_turns = Some(n);
        self
    }

    /// Set when the session started (Unix epoch seconds).
    #[must_use]
    pub fn session_started_at(mut self, ts: i64) -> Self {
        self.session_started_at = ts;
        self
    }

    /// Set when this turn started (Unix epoch seconds).
    #[must_use]
    pub fn turn_started_at(mut self, ts: i64) -> Self {
        self.turn_started_at = ts;
        self
    }

    /// Set when this turn ended (Unix epoch seconds).
    #[must_use]
    pub fn turn_ended_at(mut self, ts: i64) -> Self {
        self.turn_ended_at = ts;
        self
    }

    /// Set the wall clock duration of this turn in milliseconds.
    #[must_use]
    pub fn turn_duration_ms(mut self, ms: u64) -> Self {
        self.turn_duration_ms = ms;
        self
    }

    /// Set the prompt summary (first ~200 chars).
    #[must_use]
    pub fn prompt_summary(mut self, summary: impl Into<String>) -> Self {
        self.prompt_summary = Some(summary.into());
        self
    }

    /// Set the prompt hash (Blake3, 32 bytes).
    #[must_use]
    pub fn prompt_hash(mut self, hash: [u8; 32]) -> Self {
        self.prompt_hash = Some(hash);
        self
    }

    /// Compute and set the prompt hash from the full prompt text.
    #[must_use]
    pub fn prompt_hash_from_text(mut self, prompt: &str) -> Self {
        let hash = blake3::hash(prompt.as_bytes());
        self.prompt_hash = Some(*hash.as_bytes());
        self
    }

    /// Set the list of files modified in this turn.
    #[must_use]
    pub fn files_in_turn(mut self, files: Vec<String>) -> Self {
        self.files_in_turn = files;
        self
    }

    /// Set the cumulative unique file count across the session.
    #[must_use]
    pub fn files_in_session(mut self, count: u32) -> Self {
        self.files_in_session = count;
        self
    }

    /// Set the delegation ID for agent authorization.
    #[must_use]
    pub fn delegation_id(mut self, id: impl Into<String>) -> Self {
        self.delegation_id = Some(id.into());
        self
    }

    /// Build the `SessionEnvelope`.
    pub fn build(self) -> SessionEnvelope {
        SessionEnvelope {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id,
            agent_name: self.agent_name,
            agent_display_name: self.agent_display_name,
            turn_number: self.turn_number,
            total_turns: self.total_turns,
            session_started_at: self.session_started_at,
            turn_started_at: self.turn_started_at,
            turn_ended_at: self.turn_ended_at,
            turn_duration_ms: self.turn_duration_ms,
            prompt_summary: self.prompt_summary,
            prompt_hash: self.prompt_hash,
            files_in_turn: self.files_in_turn,
            files_in_session: self.files_in_session,
            delegation_id: self.delegation_id,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_envelope() -> SessionEnvelope {
        SessionEnvelope::builder("sess-abc-123", "claude-code")
            .agent_display_name("Claude Code")
            .turn_number(3)
            .total_turns(7)
            .session_started_at(1737000000)
            .turn_started_at(1737000300)
            .turn_ended_at(1737000312)
            .turn_duration_ms(12400)
            .prompt_summary("Fix the authentication bug in login.rs")
            .files_in_turn(vec![
                "src/auth.rs".to_string(),
                "src/auth_test.rs".to_string(),
            ])
            .files_in_session(5)
            .delegation_id("deleg-xyz")
            .build()
    }

    fn make_minimal_envelope() -> SessionEnvelope {
        SessionEnvelope::builder("s1", "test-agent").build()
    }

    // =========================================================================
    // Builder tests
    // =========================================================================

    #[test]
    fn test_builder_required_fields() {
        let e = SessionEnvelope::builder("sess-1", "claude-code").build();
        assert_eq!(e.session_id, "sess-1");
        assert_eq!(e.agent_name, "claude-code");
        assert_eq!(e.schema_version, SCHEMA_VERSION);
        assert_eq!(e.turn_number, 1); // default
        assert!(e.total_turns.is_none());
        assert!(e.prompt_summary.is_none());
        assert!(e.prompt_hash.is_none());
        assert!(e.files_in_turn.is_empty());
        assert_eq!(e.files_in_session, 0);
        assert!(e.delegation_id.is_none());
        assert!(e.agent_display_name.is_none());
    }

    #[test]
    fn test_builder_all_fields() {
        let e = make_envelope();
        assert_eq!(e.session_id, "sess-abc-123");
        assert_eq!(e.agent_name, "claude-code");
        assert_eq!(e.agent_display_name.as_deref(), Some("Claude Code"));
        assert_eq!(e.turn_number, 3);
        assert_eq!(e.total_turns, Some(7));
        assert_eq!(e.session_started_at, 1737000000);
        assert_eq!(e.turn_started_at, 1737000300);
        assert_eq!(e.turn_ended_at, 1737000312);
        assert_eq!(e.turn_duration_ms, 12400);
        assert_eq!(
            e.prompt_summary.as_deref(),
            Some("Fix the authentication bug in login.rs")
        );
        assert_eq!(e.files_in_turn.len(), 2);
        assert_eq!(e.files_in_session, 5);
        assert_eq!(e.delegation_id.as_deref(), Some("deleg-xyz"));
    }

    #[test]
    fn test_builder_prompt_hash_from_text() {
        let e = SessionEnvelope::builder("s", "a")
            .prompt_hash_from_text("Fix the bug")
            .build();

        assert!(e.prompt_hash.is_some());
        let expected = blake3::hash(b"Fix the bug");
        assert_eq!(e.prompt_hash.unwrap(), *expected.as_bytes());
    }

    #[test]
    fn test_builder_prompt_hash_direct() {
        let hash = [42u8; 32];
        let e = SessionEnvelope::builder("s", "a").prompt_hash(hash).build();

        assert_eq!(e.prompt_hash, Some(hash));
    }

    // =========================================================================
    // Encode / Decode roundtrip
    // =========================================================================

    #[test]
    fn test_encode_decode_roundtrip_full() {
        let original = make_envelope();
        let bytes = original.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_encode_decode_roundtrip_minimal() {
        let original = make_minimal_envelope();
        let bytes = original.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_encode_decode_roundtrip_with_prompt_hash() {
        let original = SessionEnvelope::builder("s", "a")
            .prompt_hash_from_text("hello world")
            .build();
        let bytes = original.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(original.prompt_hash, decoded.prompt_hash);
    }

    #[test]
    fn test_encode_has_magic_prefix() {
        let e = make_minimal_envelope();
        let bytes = e.encode().unwrap();
        assert!(bytes.len() >= MAGIC.len());
        assert_eq!(&bytes[..4], MAGIC);
    }

    #[test]
    fn test_encode_size_reasonable() {
        // A full envelope should be well under 1KB
        let e = make_envelope();
        let bytes = e.encode().unwrap();
        assert!(
            bytes.len() < 1024,
            "Encoded size {} exceeds 1KB",
            bytes.len()
        );

        // Minimal envelope should be very small
        let e = make_minimal_envelope();
        let bytes = e.encode().unwrap();
        assert!(
            bytes.len() < 256,
            "Minimal encoded size {} exceeds 256 bytes",
            bytes.len()
        );
    }

    // =========================================================================
    // Decode error cases
    // =========================================================================

    #[test]
    fn test_decode_too_short() {
        // Just magic, no payload
        let err = SessionEnvelope::decode(b"ATSE").unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("too short"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_decode_empty() {
        let err = SessionEnvelope::decode(b"").unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("too short"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_decode_three_bytes() {
        let err = SessionEnvelope::decode(b"ATS").unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("too short"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_decode_wrong_magic() {
        let err = SessionEnvelope::decode(b"XXXXmore data here").unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("invalid magic"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_decode_unsupported_version() {
        let mut bytes = make_minimal_envelope().encode().unwrap();
        // Bump version to something unsupported
        bytes[4] = 99;
        let err = SessionEnvelope::decode(&bytes).unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("unsupported schema version"));
                assert!(reason.contains("99"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_decode_corrupted_payload() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(SCHEMA_VERSION);
        bytes.extend_from_slice(b"this is not valid postcard data at all");
        let err = SessionEnvelope::decode(&bytes).unwrap_err();
        assert!(matches!(err, AgentError::EnvelopeCodecError { .. }));
    }

    // =========================================================================
    // is_session_envelope
    // =========================================================================

    #[test]
    fn test_is_session_envelope_valid() {
        let bytes = make_envelope().encode().unwrap();
        assert!(SessionEnvelope::is_session_envelope(&bytes));
    }

    #[test]
    fn test_is_session_envelope_minimal() {
        let bytes = make_minimal_envelope().encode().unwrap();
        assert!(SessionEnvelope::is_session_envelope(&bytes));
    }

    #[test]
    fn test_is_session_envelope_not_envelope() {
        assert!(!SessionEnvelope::is_session_envelope(b"not an envelope"));
        assert!(!SessionEnvelope::is_session_envelope(b""));
        assert!(!SessionEnvelope::is_session_envelope(b"ATS")); // 3 bytes
        assert!(!SessionEnvelope::is_session_envelope(b"XXXX\x01")); // wrong magic
    }

    #[test]
    fn test_is_session_envelope_other_metadata() {
        // Simulate other metadata stored in HashedChange.metadata
        let other_metadata = postcard::to_allocvec(&"some other data").unwrap();
        assert!(!SessionEnvelope::is_session_envelope(&other_metadata));
    }

    #[test]
    fn test_is_session_envelope_json_metadata() {
        // JSON metadata (another possible use of the metadata field)
        let json = serde_json::to_vec(&serde_json::json!({"key": "value"})).unwrap();
        assert!(!SessionEnvelope::is_session_envelope(&json));
    }

    // =========================================================================
    // Helper methods
    // =========================================================================

    #[test]
    fn test_turn_file_count() {
        let e = make_envelope();
        assert_eq!(e.turn_file_count(), 2);

        let e = make_minimal_envelope();
        assert_eq!(e.turn_file_count(), 0);
    }

    #[test]
    fn test_has_prompt() {
        let e = make_envelope();
        assert!(e.has_prompt());

        let e = make_minimal_envelope();
        assert!(!e.has_prompt());

        let e = SessionEnvelope::builder("s", "a")
            .prompt_summary("")
            .build();
        assert!(!e.has_prompt());
    }

    #[test]
    fn test_is_session_complete() {
        let e = make_envelope(); // has total_turns = Some(7)
        assert!(e.is_session_complete());

        let e = make_minimal_envelope(); // total_turns = None
        assert!(!e.is_session_complete());
    }

    // =========================================================================
    // Duration display
    // =========================================================================

    #[test]
    fn test_duration_display_milliseconds() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(500)
            .build();
        assert_eq!(e.duration_display(), "500ms");
    }

    #[test]
    fn test_duration_display_seconds_with_decimal() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(2500)
            .build();
        assert_eq!(e.duration_display(), "2.5s");
    }

    #[test]
    fn test_duration_display_seconds_whole() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(45_000)
            .build();
        assert_eq!(e.duration_display(), "45s");
    }

    #[test]
    fn test_duration_display_seconds_fractional_mid_range() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(12_400)
            .build();
        assert_eq!(e.duration_display(), "12.4s");
    }

    #[test]
    fn test_duration_display_minutes_and_seconds() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(125_400)
            .build();
        assert_eq!(e.duration_display(), "2m 5s");
    }

    #[test]
    fn test_duration_display_exact_minutes() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(120_000)
            .build();
        assert_eq!(e.duration_display(), "2m");
    }

    #[test]
    fn test_duration_display_zero() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(0)
            .build();
        assert_eq!(e.duration_display(), "0ms");
    }

    #[test]
    fn test_duration_display_one_second() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(1_000)
            .build();
        assert_eq!(e.duration_display(), "1s");
    }

    #[test]
    fn test_duration_display_boundary_10s() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_duration_ms(10_000)
            .build();
        assert_eq!(e.duration_display(), "10s");
    }

    // =========================================================================
    // Display trait
    // =========================================================================

    #[test]
    fn test_display_full() {
        let e = make_envelope();
        let s = e.to_string();
        assert!(s.contains("Turn 3"));
        assert!(s.contains("sess-abc-123"));
        assert!(s.contains("claude-code"));
        assert!(s.contains("12.4s"));
        assert!(s.contains("Fix the authentication bug"));
    }

    #[test]
    fn test_display_minimal() {
        let e = make_minimal_envelope();
        let s = e.to_string();
        assert!(s.contains("Turn 1"));
        assert!(s.contains("s1"));
        assert!(s.contains("test-agent"));
        assert!(!s.contains("—")); // no prompt
    }

    #[test]
    fn test_display_long_prompt_truncated() {
        let long_prompt = "a".repeat(200);
        let e = SessionEnvelope::builder("s", "a")
            .prompt_summary(long_prompt)
            .build();
        let s = e.to_string();
        assert!(s.contains("..."));
        // Full display should be bounded
        assert!(s.len() < 200);
    }

    // =========================================================================
    // Serde (JSON) roundtrip — for debugging/inspection
    // =========================================================================

    #[test]
    fn test_json_roundtrip() {
        let original = make_envelope();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SessionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_json_minimal_has_null_optionals() {
        // With serde (no skip_serializing_if),
        // None fields serialize as null in JSON and empty vecs as [].
        let e = make_minimal_envelope();
        let json = serde_json::to_string(&e).unwrap();
        // Required fields are present
        assert!(json.contains("session_id"));
        assert!(json.contains("agent_name"));
        assert!(json.contains("turn_number"));
        // Optional fields are present as null
        assert!(json.contains("total_turns"));
        assert!(json.contains("delegation_id"));
    }

    #[test]
    fn test_json_includes_present_fields() {
        let e = make_envelope();
        let json = serde_json::to_string_pretty(&e).unwrap();
        assert!(json.contains("session_id"));
        assert!(json.contains("agent_name"));
        assert!(json.contains("turn_number"));
        assert!(json.contains("total_turns"));
        assert!(json.contains("prompt_summary"));
        assert!(json.contains("files_in_turn"));
        assert!(json.contains("delegation_id"));
        assert!(json.contains("agent_display_name"));
    }

    // =========================================================================
    // Schema version
    // =========================================================================

    #[test]
    fn test_schema_version_is_1() {
        assert_eq!(SCHEMA_VERSION, 1);
        let e = make_minimal_envelope();
        assert_eq!(e.schema_version, 1);
    }

    #[test]
    fn test_schema_version_in_decoded() {
        let e = make_minimal_envelope();
        let bytes = e.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn test_future_version_rejected() {
        let mut e = make_minimal_envelope();
        e.schema_version = 99;
        // Encode with the high version baked in
        let payload = postcard::to_allocvec(&e).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&payload);
        let err = SessionEnvelope::decode(&bytes).unwrap_err();
        match err {
            AgentError::EnvelopeCodecError { reason } => {
                assert!(reason.contains("unsupported schema version"));
                assert!(reason.contains("99"));
            }
            other => panic!("Expected EnvelopeCodecError, got: {:?}", other),
        }
    }

    #[test]
    fn test_version_0_accepted() {
        let mut e = make_minimal_envelope();
        e.schema_version = 0;
        let payload = postcard::to_allocvec(&e).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&payload);
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.schema_version, 0);
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn test_empty_session_id() {
        let e = SessionEnvelope::builder("", "agent").build();
        let bytes = e.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.session_id, "");
    }

    #[test]
    fn test_large_files_list() {
        let files: Vec<String> = (0..1000).map(|i| format!("src/file_{}.rs", i)).collect();
        let e = SessionEnvelope::builder("s", "a")
            .files_in_turn(files.clone())
            .build();
        let bytes = e.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.files_in_turn.len(), 1000);
        assert_eq!(decoded.files_in_turn, files);
    }

    #[test]
    fn test_unicode_prompt_summary() {
        let prompt = "修复认证模块中的缺陷 🐛";
        let e = SessionEnvelope::builder("s", "a")
            .prompt_summary(prompt)
            .build();
        let bytes = e.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.prompt_summary.as_deref(), Some(prompt));
    }

    #[test]
    fn test_max_turn_number() {
        let e = SessionEnvelope::builder("s", "a")
            .turn_number(u32::MAX)
            .build();
        let bytes = e.encode().unwrap();
        let decoded = SessionEnvelope::decode(&bytes).unwrap();
        assert_eq!(decoded.turn_number, u32::MAX);
    }

    #[test]
    fn test_clone_eq() {
        let e = make_envelope();
        let cloned = e.clone();
        assert_eq!(e, cloned);
    }
}

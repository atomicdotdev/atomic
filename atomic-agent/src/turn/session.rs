//! Agent session state and persistence.
//!
//! This module provides the [`AgentSession`] struct representing an active or
//! ended agent session, and [`SessionStore`] for persisting session state as
//! JSON files in `.atomic/sessions/`.
//!
//! # Storage Layout
//!
//! ```text
//! .atomic/
//! └── sessions/
//!     ├── 2026-01-15-abc123de.json     # Active session
//!     ├── 2026-01-15-def456gh.json     # Ended session
//!     └── ...
//! ```
//!
//! Each session file contains a JSON-serialized [`AgentSession`]. Files are
//! written atomically (temp file + rename) to prevent corruption from crashes.
//!
//! # Session Lifecycle
//!
//! ```text
//! SessionStart hook → create AgentSession (phase: Idle)
//!     │
//!     ├─ TurnStart → phase: Active, increment turn_count
//!     │   └─ TurnEnd → phase: Idle, record change
//!     │
//!     ├─ TurnStart → phase: Active (turn 2)
//!     │   └─ TurnEnd → phase: Idle, record change
//!     │
//!     └─ SessionEnd hook → phase: Ended, set ended_at
//!
//! Later: SessionStart with same ID → phase: Idle (re-enter)
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use atomic_agent::turn::session::{AgentSession, SessionStore};
//!
//! let store = SessionStore::new("/path/to/repo/.atomic/sessions").unwrap();
//!
//! // Create a new session
//! let session = AgentSession::new("sess-abc-123", "claude-code", "Claude Code");
//! store.save(&session).unwrap();
//!
//! // Load it back
//! let loaded = store.load("sess-abc-123").unwrap();
//! assert!(loaded.is_some());
//!
//! // List all sessions
//! let all = store.list().unwrap();
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};
use crate::turn::phase::Phase;

// ManagedRunStamp

/// Correlation stamp for sessions born under a managed lifecycle.
///
/// When an outer orchestrator (e.g. Sherpa/noname) declares a run via
/// `atomic agent lifecycle begin`, every session created inside that run's
/// workdir is stamped with the run context. The stamp is the queryable edge
/// between the orchestrator's run and the sessions/changes it produced —
/// the PROV `actedOnBehalfOf` chain: this agent's activity was performed
/// on behalf of `owner_agent`'s run `run_id`.
///
/// Sessions that already existed when the lifecycle began are NOT stamped —
/// the stamp marks sessions born within the run, so a user's own concurrent
/// direct session is never attributed to the orchestrator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRunStamp {
    /// The managed run id (from `lifecycle begin`).
    pub run_id: String,

    /// The lifecycle owner agent (e.g. "sherpa").
    pub owner_agent: String,

    /// The owner's own session id for the run.
    pub owner_session_id: String,

    /// Optional work item the owner associated with the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
}

// AgentSession

/// State of an agent session.
///
/// Persisted as JSON in `.atomic/sessions/{session_id}.json`. Updated on every
/// hook callback via the orchestrator. The session links to an Atomic view
/// named `agent-{session_id}` where turn changes are recorded.
///
/// # Identity
///
/// Each session is uniquely identified by `session_id`, which is assigned by the
/// agent (e.g., a UUID from Claude Code). The `view_name` is derived as
/// `agent-{session_id}`.
///
/// # Thread Safety
///
/// `AgentSession` is not thread-safe. The orchestrator holds exclusive access
/// during hook processing. Concurrent sessions in different processes use
/// separate session IDs and separate files.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSession {
    /// Unique session identifier (assigned by the agent).
    ///
    /// Format varies by agent — UUID for Claude Code, path-derived for Gemini CLI.
    pub session_id: String,

    /// The Atomic view name for this session's turn changes.
    ///
    /// Format: `agent-{session_id}`. Created on first turn recording.
    #[serde(alias = "stack_name")]
    pub view_name: String,

    /// Current lifecycle phase.
    pub phase: Phase,

    /// Number of turns completed in this session.
    ///
    /// Incremented after each successful turn recording. Turn 1 is the first
    /// turn after the session starts.
    pub turn_count: u32,

    /// Agent registry key (e.g., "claude-code", "gemini-cli").
    pub agent_name: String,

    /// Human-readable agent name (e.g., "Claude Code", "Gemini CLI").
    pub agent_display_name: String,

    /// AI vendor identifier (e.g., "anthropic", "google").
    #[serde(default)]
    pub agent_vendor: String,

    /// AI model identifier (e.g., "claude-sonnet-4-20250514").
    #[serde(default)]
    pub model: String,

    /// When the session was created.
    pub started_at: DateTime<Utc>,

    /// When the session was last interacted with (any hook callback).
    ///
    /// Updated by `apply_common_actions` via the `SessionState` trait impl.
    /// Used for stale session detection in `atomic agent status`.
    #[serde(default)]
    pub last_interaction: Option<DateTime<Utc>>,

    /// When the session ended (agent process exited).
    ///
    /// `None` while the session is active. Set when `SessionStop` is received.
    /// Cleared if the session is re-entered via `SessionStart`.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,

    /// The view that was current when this session started.
    ///
    /// The agent view is forked from this view so it inherits all existing
    /// changes (e.g., `.atomicignore`, project config). When the session ends,
    /// changes can be applied back to this view.
    ///
    /// Set during `handle_session_start` by reading the repository's current
    /// view. `None` for sessions created before this field was added.
    #[serde(default, alias = "parent_stack")]
    pub parent_view: Option<String>,

    /// Path to the agent's transcript file, if known.
    ///
    /// Set from the first hook that includes `transcript_path`.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,

    /// The first user prompt in this session (truncated for display).
    ///
    /// Set from the first `TurnStart` event that includes a prompt.
    /// Truncated to 200 characters.
    #[serde(default)]
    pub first_prompt: Option<String>,

    /// The current turn's user prompt (updated on every `TurnStart`).
    ///
    /// Unlike `first_prompt` which is set-once, this is overwritten on each
    /// new turn so the change message reflects the prompt that triggered
    /// THIS turn, not the session's opening prompt.
    /// Cleared after recording via `clear_current_prompt()`.
    #[serde(default)]
    pub current_prompt: Option<String>,

    /// Accumulated list of files touched across all turns.
    ///
    /// Deduplicated. Used for session-level reporting and for the
    /// `files_in_session` count in `SessionEnvelope`.
    #[serde(default)]
    pub files_touched: Vec<String>,

    /// When the current turn started (if a turn is active).
    ///
    /// Set on `TurnStart`, cleared on `TurnEnd`. Used to compute turn duration
    /// for the `SessionEnvelope`.
    #[serde(default)]
    pub current_turn_started_at: Option<DateTime<Utc>>,

    /// Change hashes the orchestrator actually recorded for this session.
    ///
    /// Appended on each successful `record_turn()` (see `handle_turn_end`).
    /// Used by `create_session_attestation` as the authoritative coverage set
    /// — replaces the previous "scan the whole agent-view history" path, which
    /// over-counted inherited baseline changes from the parent view.
    ///
    /// `#[serde(default)]` keeps existing session JSON files (written before
    /// this field existed) loadable as an empty vec.
    #[serde(default)]
    pub recorded_change_hashes: Vec<atomic_core::types::Hash>,

    /// Managed-run stamp, set when this session was created under a managed
    /// lifecycle (see [`ManagedRunStamp`]). `None` for direct agent sessions.
    ///
    /// `atomic agent lifecycle end --json` scans session files for this stamp
    /// to build the run summary (sessions, views, recorded change hashes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_run: Option<ManagedRunStamp>,
}

impl AgentSession {
    /// Maximum length for the stored first_prompt.
    const MAX_PROMPT_LENGTH: usize = 200;

    /// Create a new session in the `Idle` phase.
    ///
    /// # Arguments
    ///
    /// * `session_id` — Unique session identifier from the agent
    /// * `agent_name` — Agent registry key (e.g., "claude-code")
    /// * `agent_display_name` — Human-readable name (e.g., "Claude Code")
    pub fn new(
        session_id: impl Into<String>,
        agent_name: impl Into<String>,
        agent_display_name: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        let view_name = Self::make_view_name(&session_id);
        let now = Utc::now();

        Self {
            session_id,
            view_name,
            phase: Phase::Idle,
            turn_count: 0,
            agent_name: agent_name.into(),
            agent_display_name: agent_display_name.into(),
            agent_vendor: String::new(),
            model: String::new(),
            started_at: now,
            last_interaction: Some(now),
            ended_at: None,
            parent_view: None,
            transcript_path: None,
            first_prompt: None,
            current_prompt: None,
            files_touched: Vec::new(),
            current_turn_started_at: None,
            recorded_change_hashes: Vec::new(),
            managed_run: None,
        }
    }

    /// Derive the Atomic view name for an agent session.
    ///
    /// Uses haikunator-style names (e.g., "bold-creek-a3f2") from the
    /// same generator as vault goals. The `session_id` is not used in the
    /// name — it's stored separately on the session struct.
    pub fn make_view_name(_session_id: &str) -> String {
        atomic_repository::generate_goal_name()
    }

    /// Set the AI vendor and model.
    pub fn set_model_info(&mut self, vendor: impl Into<String>, model: impl Into<String>) {
        self.agent_vendor = vendor.into();
        self.model = model.into();
    }

    /// Set the parent view name (the view that was current when this session started).
    ///
    /// Only sets the value if it hasn't been set already (first call wins).
    pub fn set_parent_view(&mut self, view: impl Into<String>) {
        if self.parent_view.is_none() {
            self.parent_view = Some(view.into());
        }
    }

    /// Get the parent view name, if set.
    pub fn parent_view(&self) -> Option<&str> {
        self.parent_view.as_deref()
    }

    /// Set the transcript path if not already set.
    pub fn set_transcript_path(&mut self, path: impl Into<PathBuf>) {
        if self.transcript_path.is_none() {
            self.transcript_path = Some(path.into());
        }
    }

    /// Clear the current prompt after recording a turn.
    ///
    /// Called after `record_turn` so that if the agent continues without
    /// a new user prompt, the next turn doesn't reuse the old one.
    pub fn clear_current_prompt(&mut self) {
        self.current_prompt = None;
    }

    /// Set the first prompt if not already set. Truncates to MAX_PROMPT_LENGTH.
    /// Also updates `current_prompt` on every call so the change message
    /// reflects this turn's intent, not the session's opening prompt.
    pub fn set_first_prompt(&mut self, prompt: &str) {
        if prompt.is_empty() {
            return;
        }

        let stored = if prompt.len() <= Self::MAX_PROMPT_LENGTH {
            prompt.to_string()
        } else {
            let truncated: String = prompt.chars().take(Self::MAX_PROMPT_LENGTH - 3).collect();
            format!("{}...", truncated)
        };

        // first_prompt is set-once (preserves the session's opening prompt)
        if self.first_prompt.is_none() {
            self.first_prompt = Some(stored.clone());
        }

        // current_prompt is updated every turn so the change message
        // reflects THIS turn's intent, not the session's opening prompt.
        self.current_prompt = Some(stored);
    }

    /// Add files to the accumulated files_touched list (deduplicating).
    pub fn add_files_touched(&mut self, files: &[String]) {
        for file in files {
            if !self.files_touched.contains(file) {
                self.files_touched.push(file.clone());
            }
        }
    }

    /// Returns the number of unique files touched across the session.
    pub fn files_touched_count(&self) -> u32 {
        self.files_touched.len() as u32
    }

    /// Mark the start of a new turn.
    pub fn begin_turn(&mut self) {
        self.current_turn_started_at = Some(Utc::now());
    }

    /// Mark the end of the current turn and increment the turn count.
    ///
    /// Returns the turn number that just completed (1-indexed).
    pub fn end_turn(&mut self) -> u32 {
        self.turn_count += 1;
        self.current_turn_started_at = None;
        self.turn_count
    }

    /// Returns the duration of the current turn in milliseconds, if a turn is active.
    pub fn current_turn_duration_ms(&self) -> Option<u64> {
        self.current_turn_started_at.map(|started| {
            let duration = Utc::now().signed_duration_since(started);
            duration.num_milliseconds().max(0) as u64
        })
    }

    /// Returns `true` if the session has ended.
    pub fn is_ended(&self) -> bool {
        self.phase.is_ended()
    }

    /// Returns `true` if the session has an active turn.
    pub fn is_turn_active(&self) -> bool {
        self.phase.is_active()
    }

    /// Returns the session duration as a human-readable string.
    pub fn duration_display(&self) -> String {
        let end = self.ended_at.unwrap_or_else(Utc::now);
        let duration = end.signed_duration_since(self.started_at);
        let secs = duration.num_seconds();

        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            let hours = secs / 3600;
            let minutes = (secs % 3600) / 60;
            format!("{}h {}m", hours, minutes)
        }
    }
}

impl fmt::Display for AgentSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Session {} ({}, {}, {} turn{}, {})",
            self.session_id,
            self.agent_display_name,
            self.phase,
            self.turn_count,
            if self.turn_count == 1 { "" } else { "s" },
            self.duration_display(),
        )?;
        if let Some(ref prompt) = self.first_prompt {
            let display = if prompt.len() > 50 {
                let truncated: String = prompt.chars().take(47).collect();
                format!("{}...", truncated)
            } else {
                prompt.clone()
            };
            write!(f, " — \"{}\"", display)?;
        }
        Ok(())
    }
}

// SessionState trait implementation

impl super::phase::SessionState for AgentSession {
    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        if phase.is_ended() && self.ended_at.is_none() {
            self.ended_at = Some(Utc::now());
        }
    }

    fn touch_interaction(&mut self) {
        self.last_interaction = Some(Utc::now());
    }

    fn clear_ended_at(&mut self) {
        self.ended_at = None;
    }
}

// SessionStore

/// Persistent storage for agent session state files.
///
/// Sessions are stored as `{session_id}.json` files in the sessions directory
/// (typically `.atomic/sessions/`). All writes are atomic (temp file + rename).
///
/// # Path Safety
///
/// Session IDs are validated to prevent path traversal attacks. IDs containing
/// `..`, `/`, or `\` are rejected.
pub struct SessionStore {
    /// Directory where session state files are stored.
    sessions_dir: PathBuf,
}

impl SessionStore {
    /// Create a new session store at the given directory path.
    ///
    /// Creates the directory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `sessions_dir` — Path to the sessions directory (e.g., `.atomic/sessions`)
    pub fn new(sessions_dir: impl Into<PathBuf>) -> AgentResult<Self> {
        let sessions_dir = sessions_dir.into();
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self { sessions_dir })
    }

    /// Create a session store for a repository at the given root path.
    ///
    /// The sessions directory is `{repo_root}/.atomic/sessions/`.
    pub fn for_repo(repo_root: &Path) -> AgentResult<Self> {
        Self::new(repo_root.join(".atomic").join("sessions"))
    }

    /// Load a session by ID.
    ///
    /// Returns `Ok(None)` if the session file doesn't exist (not an error).
    ///
    /// # Errors
    ///
    /// Returns `AgentError::SessionIdInvalid` if the ID contains path traversal characters.
    /// Returns `AgentError::SessionLoadFailed` if the file exists but can't be parsed.
    pub fn load(&self, session_id: &str) -> AgentResult<Option<AgentSession>> {
        validate_session_id(session_id)?;

        let path = self.session_path(session_id);

        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(AgentError::SessionLoadFailed {
                    session_id: session_id.to_string(),
                    reason: e.to_string(),
                });
            }
        };

        let session: AgentSession =
            serde_json::from_slice(&data).map_err(|e| AgentError::SessionLoadFailed {
                session_id: session_id.to_string(),
                reason: format!("JSON parse error: {}", e),
            })?;

        Ok(Some(session))
    }

    /// Save a session to disk atomically.
    ///
    /// Writes to a temporary file first, then renames to the final path.
    /// This prevents corruption if the process is killed mid-write.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::SessionSaveFailed` if the write or rename fails.
    pub fn save(&self, session: &AgentSession) -> AgentResult<()> {
        validate_session_id(&session.session_id)?;

        let path = self.session_path(&session.session_id);
        let tmp_path = path.with_extension("json.tmp");

        let data =
            serde_json::to_string_pretty(session).map_err(|e| AgentError::SessionSaveFailed {
                session_id: session.session_id.clone(),
                reason: format!("JSON serialize error: {}", e),
            })?;

        // Write to temp file
        std::fs::write(&tmp_path, data.as_bytes()).map_err(|e| AgentError::SessionSaveFailed {
            session_id: session.session_id.clone(),
            reason: format!("write temp file: {}", e),
        })?;

        // Atomic rename
        std::fs::rename(&tmp_path, &path).map_err(|e| AgentError::SessionSaveFailed {
            session_id: session.session_id.clone(),
            reason: format!("rename temp file: {}", e),
        })?;

        Ok(())
    }

    /// Delete a session state file.
    ///
    /// Returns `Ok(())` if the file doesn't exist (idempotent).
    pub fn clear(&self, session_id: &str) -> AgentResult<()> {
        validate_session_id(session_id)?;

        let path = self.session_path(session_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AgentError::SessionSaveFailed {
                session_id: session_id.to_string(),
                reason: format!("delete: {}", e),
            }),
        }
    }

    /// List all stored sessions.
    ///
    /// Returns sessions sorted by `started_at` (newest first).
    /// Skips corrupted session files with a warning log.
    pub fn list(&self) -> AgentResult<Vec<AgentSession>> {
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut sessions = Vec::new();

        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Only process .json files (skip .tmp files, etc.)
            if !name_str.ends_with(".json") || name_str.ends_with(".json.tmp") {
                continue;
            }

            let session_id = name_str.trim_end_matches(".json");

            match self.load(session_id) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {} // File disappeared between readdir and load
                Err(e) => {
                    log::warn!("Skipping corrupted session file {}: {}", name_str, e);
                }
            }
        }

        // Sort by started_at, newest first
        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));

        Ok(sessions)
    }

    /// Find all active (non-ended) sessions.
    ///
    /// Returns sessions where `phase != Ended`.
    pub fn find_active(&self) -> AgentResult<Vec<AgentSession>> {
        let all = self.list()?;
        Ok(all.into_iter().filter(|s| !s.is_ended()).collect())
    }

    /// Find all ended sessions.
    pub fn find_ended(&self) -> AgentResult<Vec<AgentSession>> {
        let all = self.list()?;
        Ok(all.into_iter().filter(|s| s.is_ended()).collect())
    }

    /// Returns the number of stored session files.
    pub fn count(&self) -> AgentResult<usize> {
        Ok(self.list()?.len())
    }

    /// Returns the path to a session's JSON file.
    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.json", session_id))
    }

    /// Returns the sessions directory path.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }
}

impl fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionStore")
            .field("sessions_dir", &self.sessions_dir)
            .finish()
    }
}

// Validation

/// Validate a session ID to prevent path traversal.
///
/// Rejects IDs containing `..`, `/`, `\`, or null bytes.
fn validate_session_id(session_id: &str) -> AgentResult<()> {
    if session_id.is_empty() {
        return Err(AgentError::SessionIdInvalid {
            session_id: "(empty)".to_string(),
        });
    }

    if session_id.contains("..")
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0')
    {
        return Err(AgentError::SessionIdInvalid {
            session_id: session_id.to_string(),
        });
    }

    // Reject very long session IDs (filesystem path limits)
    if session_id.len() > 255 {
        return Err(AgentError::SessionIdInvalid {
            session_id: format!(
                "{}... (too long: {} chars)",
                &session_id[..20],
                session_id.len()
            ),
        });
    }

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::phase::SessionState;
    use tempfile::TempDir;

    fn make_session() -> AgentSession {
        AgentSession::new("sess-abc-123", "claude-code", "Claude Code")
    }

    fn make_store() -> (TempDir, SessionStore) {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::new(dir.path().join("sessions")).unwrap();
        (dir, store)
    }

    // AgentSession construction

    #[test]
    fn test_new_session() {
        let s = make_session();
        assert_eq!(s.session_id, "sess-abc-123");
        assert_eq!(s.agent_name, "claude-code");
        assert_eq!(s.agent_display_name, "Claude Code");
        // View name is haikunator-style: adjective-noun-hex
        let parts: Vec<&str> = s.view_name.split('-').collect();
        assert_eq!(
            parts.len(),
            3,
            "view name '{}' should be adj-noun-hex",
            s.view_name
        );
        assert_eq!(parts[2].len(), 4);
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.turn_count, 0);
        assert!(s.last_interaction.is_some());
        assert!(s.ended_at.is_none());
        assert!(s.transcript_path.is_none());
        assert!(s.first_prompt.is_none());
        assert!(s.files_touched.is_empty());
        assert!(s.current_turn_started_at.is_none());
    }

    #[test]
    fn test_make_view_name() {
        let name = AgentSession::make_view_name("sess-123");
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(
            parts.len(),
            3,
            "view name '{}' should be adj-noun-hex",
            name
        );
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
        // Different calls produce different names
        let name2 = AgentSession::make_view_name("sess-456");
        // Could theoretically collide but astronomically unlikely
        // Just verify format is correct
        let parts2: Vec<&str> = name2.split('-').collect();
        assert_eq!(parts2.len(), 3);
    }

    // Model info

    #[test]
    fn test_set_model_info() {
        let mut s = make_session();
        s.set_model_info("anthropic", "claude-sonnet-4-20250514");
        assert_eq!(s.agent_vendor, "anthropic");
        assert_eq!(s.model, "claude-sonnet-4-20250514");
    }

    // Transcript path

    #[test]
    fn test_set_transcript_path_first_time() {
        let mut s = make_session();
        s.set_transcript_path("/tmp/t.jsonl");
        assert_eq!(s.transcript_path, Some(PathBuf::from("/tmp/t.jsonl")));
    }

    #[test]
    fn test_set_transcript_path_no_overwrite() {
        let mut s = make_session();
        s.set_transcript_path("/first");
        s.set_transcript_path("/second");
        assert_eq!(s.transcript_path, Some(PathBuf::from("/first")));
    }

    // First prompt

    #[test]
    fn test_set_first_prompt() {
        let mut s = make_session();
        s.set_first_prompt("Fix the bug");
        assert_eq!(s.first_prompt.as_deref(), Some("Fix the bug"));
    }

    #[test]
    fn test_set_first_prompt_no_overwrite() {
        let mut s = make_session();
        s.set_first_prompt("First prompt");
        s.set_first_prompt("Second prompt");
        assert_eq!(s.first_prompt.as_deref(), Some("First prompt"));
    }

    #[test]
    fn test_set_first_prompt_empty_ignored() {
        let mut s = make_session();
        s.set_first_prompt("");
        assert!(s.first_prompt.is_none());
    }

    #[test]
    fn test_set_first_prompt_truncated() {
        let mut s = make_session();
        let long_prompt = "a".repeat(300);
        s.set_first_prompt(&long_prompt);
        let stored = s.first_prompt.unwrap();
        assert!(stored.len() <= AgentSession::MAX_PROMPT_LENGTH);
        assert!(stored.ends_with("..."));
    }

    // Files touched

    #[test]
    fn test_add_files_touched() {
        let mut s = make_session();
        s.add_files_touched(&["a.rs".to_string(), "b.rs".to_string()]);
        assert_eq!(s.files_touched, vec!["a.rs", "b.rs"]);
        assert_eq!(s.files_touched_count(), 2);
    }

    #[test]
    fn test_add_files_touched_dedup() {
        let mut s = make_session();
        s.add_files_touched(&["a.rs".to_string(), "b.rs".to_string()]);
        s.add_files_touched(&["b.rs".to_string(), "c.rs".to_string()]);
        assert_eq!(s.files_touched, vec!["a.rs", "b.rs", "c.rs"]);
        assert_eq!(s.files_touched_count(), 3);
    }

    #[test]
    fn test_add_files_touched_empty() {
        let mut s = make_session();
        s.add_files_touched(&[]);
        assert!(s.files_touched.is_empty());
    }

    // Turn lifecycle

    #[test]
    fn test_begin_turn() {
        let mut s = make_session();
        assert!(s.current_turn_started_at.is_none());
        s.begin_turn();
        assert!(s.current_turn_started_at.is_some());
    }

    #[test]
    fn test_end_turn() {
        let mut s = make_session();
        assert_eq!(s.turn_count, 0);

        s.begin_turn();
        let turn_num = s.end_turn();
        assert_eq!(turn_num, 1);
        assert_eq!(s.turn_count, 1);
        assert!(s.current_turn_started_at.is_none());

        s.begin_turn();
        let turn_num = s.end_turn();
        assert_eq!(turn_num, 2);
        assert_eq!(s.turn_count, 2);
    }

    #[test]
    fn test_current_turn_duration_ms() {
        let mut s = make_session();
        assert!(s.current_turn_duration_ms().is_none());

        s.begin_turn();
        let duration = s.current_turn_duration_ms();
        assert!(duration.is_some());
        // Should be very small (just created)
        assert!(duration.unwrap() < 1000);
    }

    // State queries

    #[test]
    fn test_is_ended() {
        let mut s = make_session();
        assert!(!s.is_ended());
        s.phase = Phase::Ended;
        assert!(s.is_ended());
    }

    #[test]
    fn test_is_turn_active() {
        let mut s = make_session();
        assert!(!s.is_turn_active());
        s.phase = Phase::Active;
        assert!(s.is_turn_active());
        s.phase = Phase::ActiveRecorded;
        assert!(s.is_turn_active());
        s.phase = Phase::Idle;
        assert!(!s.is_turn_active());
    }

    // Display

    #[test]
    fn test_display() {
        let s = make_session();
        let display = s.to_string();
        assert!(display.contains("sess-abc-123"));
        assert!(display.contains("Claude Code"));
        assert!(display.contains("idle"));
        assert!(display.contains("0 turns"));
    }

    #[test]
    fn test_display_with_prompt() {
        let mut s = make_session();
        s.set_first_prompt("Fix the authentication bug");
        let display = s.to_string();
        assert!(display.contains("Fix the authentication bug"));
    }

    #[test]
    fn test_display_singular_turn() {
        let mut s = make_session();
        s.turn_count = 1;
        let display = s.to_string();
        assert!(display.contains("1 turn,"));
        assert!(!display.contains("1 turns"));
    }

    // SessionState trait

    #[test]
    fn test_session_state_set_phase() {
        let mut s = make_session();
        s.set_phase(Phase::Active);
        assert_eq!(s.phase, Phase::Active);
    }

    #[test]
    fn test_session_state_set_phase_ended_sets_ended_at() {
        let mut s = make_session();
        assert!(s.ended_at.is_none());
        s.set_phase(Phase::Ended);
        assert!(s.ended_at.is_some());
    }

    #[test]
    fn test_session_state_set_phase_ended_no_overwrite() {
        let mut s = make_session();
        s.set_phase(Phase::Ended);
        let first_ended = s.ended_at.unwrap();

        // Setting to Ended again shouldn't overwrite the timestamp
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.set_phase(Phase::Ended);
        assert_eq!(s.ended_at.unwrap(), first_ended);
    }

    #[test]
    fn test_session_state_touch_interaction() {
        let mut s = make_session();
        let before = s.last_interaction;

        std::thread::sleep(std::time::Duration::from_millis(10));
        s.touch_interaction();

        assert!(s.last_interaction > before);
    }

    #[test]
    fn test_session_state_clear_ended_at() {
        let mut s = make_session();
        s.set_phase(Phase::Ended);
        assert!(s.ended_at.is_some());

        s.clear_ended_at();
        assert!(s.ended_at.is_none());
    }

    // Serde roundtrip

    #[test]
    fn test_serde_roundtrip() {
        let mut s = make_session();
        s.set_model_info("anthropic", "claude-sonnet-4");
        s.set_transcript_path("/tmp/t.jsonl");
        s.set_first_prompt("Test prompt");
        s.add_files_touched(&["a.rs".to_string()]);
        s.begin_turn();
        s.end_turn();

        let json = serde_json::to_string_pretty(&s).unwrap();
        let loaded: AgentSession = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.session_id, s.session_id);
        assert_eq!(loaded.view_name, s.view_name);
        assert_eq!(loaded.phase, s.phase);
        assert_eq!(loaded.turn_count, s.turn_count);
        assert_eq!(loaded.agent_name, s.agent_name);
        assert_eq!(loaded.agent_display_name, s.agent_display_name);
        assert_eq!(loaded.agent_vendor, s.agent_vendor);
        assert_eq!(loaded.model, s.model);
        assert_eq!(loaded.transcript_path, s.transcript_path);
        assert_eq!(loaded.first_prompt, s.first_prompt);
        assert_eq!(loaded.files_touched, s.files_touched);
    }

    #[test]
    fn test_serde_backward_compat_missing_fields() {
        // Simulate an older session file missing optional fields
        let json = r#"{
            "session_id": "old-sess",
            "view_name": "bold-creek-a3f2",
            "phase": "idle",
            "turn_count": 5,
            "agent_name": "claude-code",
            "agent_display_name": "Claude Code",
            "started_at": "2026-01-15T10:00:00Z"
        }"#;

        let loaded: AgentSession = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.session_id, "old-sess");
        assert_eq!(loaded.turn_count, 5);
        assert!(loaded.agent_vendor.is_empty());
        assert!(loaded.model.is_empty());
        assert!(loaded.last_interaction.is_none());
        assert!(loaded.ended_at.is_none());
        assert!(loaded.transcript_path.is_none());
        assert!(loaded.first_prompt.is_none());
        assert!(loaded.files_touched.is_empty());
        // recorded_change_hashes was added later — old session files must
        // still load with this field as an empty vec (#[serde(default)])
        assert!(loaded.recorded_change_hashes.is_empty());
        // managed_run was added later — old session files load with None
        assert!(loaded.managed_run.is_none());
    }

    #[test]
    fn test_serde_roundtrip_with_managed_run_stamp() {
        let mut s = make_session();
        s.managed_run = Some(ManagedRunStamp {
            run_id: "run-abc".to_string(),
            owner_agent: "sherpa".to_string(),
            owner_session_id: "sherpa-sess-1".to_string(),
            work_item_id: Some("NONA-12".to_string()),
        });

        let json = serde_json::to_string_pretty(&s).unwrap();
        let loaded: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.managed_run, s.managed_run);
    }

    #[test]
    fn test_managed_run_omitted_when_none() {
        let s = make_session();
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("managed_run"),
            "managed_run must be omitted for direct sessions, got: {}",
            json
        );
    }

    #[test]
    fn test_serde_roundtrip_with_recorded_hashes() {
        use atomic_core::types::Hash;

        let mut s = make_session();
        // Push a couple of hashes (use deterministic content for stability)
        s.recorded_change_hashes.push(Hash::of(b"change-a"));
        s.recorded_change_hashes.push(Hash::of(b"change-b"));

        let json = serde_json::to_string_pretty(&s).unwrap();
        let loaded: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.recorded_change_hashes.len(), 2);
        assert_eq!(loaded.recorded_change_hashes, s.recorded_change_hashes);
    }

    // Validation

    #[test]
    fn test_validate_session_id_valid() {
        assert!(validate_session_id("sess-abc-123").is_ok());
        assert!(validate_session_id("2026-01-15-abc123de").is_ok());
        assert!(validate_session_id("simple").is_ok());
        assert!(validate_session_id("with_underscore").is_ok());
        assert!(validate_session_id("with.dots").is_ok());
    }

    #[test]
    fn test_validate_session_id_empty() {
        let err = validate_session_id("").unwrap_err();
        assert!(matches!(err, AgentError::SessionIdInvalid { .. }));
    }

    #[test]
    fn test_validate_session_id_path_traversal() {
        assert!(validate_session_id("../etc/passwd").is_err());
        assert!(validate_session_id("foo/../bar").is_err());
        assert!(validate_session_id("foo/bar").is_err());
        assert!(validate_session_id("foo\\bar").is_err());
        assert!(validate_session_id("foo\0bar").is_err());
    }

    #[test]
    fn test_validate_session_id_too_long() {
        let long_id = "a".repeat(256);
        let err = validate_session_id(&long_id).unwrap_err();
        assert!(matches!(err, AgentError::SessionIdInvalid { .. }));
    }

    // SessionStore: save / load

    #[test]
    fn test_store_save_and_load() {
        let (_dir, store) = make_store();
        let session = make_session();

        store.save(&session).unwrap();

        let loaded = store.load("sess-abc-123").unwrap();
        assert!(loaded.is_some());

        let loaded = loaded.unwrap();
        assert_eq!(loaded.session_id, "sess-abc-123");
        assert_eq!(loaded.agent_name, "claude-code");
    }

    #[test]
    fn test_store_load_nonexistent() {
        let (_dir, store) = make_store();
        let loaded = store.load("does-not-exist").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_store_save_overwrites() {
        let (_dir, store) = make_store();

        let mut session = make_session();
        session.turn_count = 1;
        store.save(&session).unwrap();

        session.turn_count = 5;
        store.save(&session).unwrap();

        let loaded = store.load("sess-abc-123").unwrap().unwrap();
        assert_eq!(loaded.turn_count, 5);
    }

    #[test]
    fn test_store_load_rejects_path_traversal() {
        let (_dir, store) = make_store();
        let err = store.load("../etc/passwd").unwrap_err();
        assert!(matches!(err, AgentError::SessionIdInvalid { .. }));
    }

    #[test]
    fn test_store_save_rejects_path_traversal() {
        let (_dir, store) = make_store();
        let mut session = make_session();
        session.session_id = "../evil".to_string();
        let err = store.save(&session).unwrap_err();
        assert!(matches!(err, AgentError::SessionIdInvalid { .. }));
    }

    // SessionStore: clear

    #[test]
    fn test_store_clear() {
        let (_dir, store) = make_store();

        store.save(&make_session()).unwrap();
        assert!(store.load("sess-abc-123").unwrap().is_some());

        store.clear("sess-abc-123").unwrap();
        assert!(store.load("sess-abc-123").unwrap().is_none());
    }

    #[test]
    fn test_store_clear_nonexistent_is_ok() {
        let (_dir, store) = make_store();
        assert!(store.clear("does-not-exist").is_ok());
    }

    // SessionStore: list

    #[test]
    fn test_store_list_empty() {
        let (_dir, store) = make_store();
        let sessions = store.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_store_list_multiple() {
        let (_dir, store) = make_store();

        let s1 = AgentSession::new("sess-1", "claude-code", "Claude Code");
        let s2 = AgentSession::new("sess-2", "gemini-cli", "Gemini CLI");

        store.save(&s1).unwrap();
        store.save(&s2).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_store_list_sorted_newest_first() {
        let (_dir, store) = make_store();

        // Create sessions with slightly different timestamps
        let mut s1 = AgentSession::new("sess-1", "a", "A");
        s1.started_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store.save(&s1).unwrap();

        let mut s2 = AgentSession::new("sess-2", "b", "B");
        s2.started_at = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store.save(&s2).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "sess-2"); // newer first
        assert_eq!(sessions[1].session_id, "sess-1");
    }

    #[test]
    fn test_store_list_skips_tmp_files() {
        let (_dir, store) = make_store();

        store.save(&make_session()).unwrap();

        // Create a .tmp file that should be skipped
        std::fs::write(
            store.sessions_dir().join("crash-in-progress.json.tmp"),
            "{}",
        )
        .unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn test_store_list_skips_corrupted_files() {
        let (_dir, store) = make_store();

        store.save(&make_session()).unwrap();

        // Create a corrupted session file
        std::fs::write(
            store.sessions_dir().join("corrupted.json"),
            "not valid json at all",
        )
        .unwrap();

        let sessions = store.list().unwrap();
        // Should still return the valid session
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-abc-123");
    }

    // SessionStore: find_active / find_ended

    #[test]
    fn test_store_find_active() {
        let (_dir, store) = make_store();

        let active = AgentSession::new("active-1", "a", "A");
        let mut ended = AgentSession::new("ended-1", "b", "B");
        ended.phase = Phase::Ended;
        ended.ended_at = Some(Utc::now());

        store.save(&active).unwrap();
        store.save(&ended).unwrap();

        let active_sessions = store.find_active().unwrap();
        assert_eq!(active_sessions.len(), 1);
        assert_eq!(active_sessions[0].session_id, "active-1");
    }

    #[test]
    fn test_store_find_ended() {
        let (_dir, store) = make_store();

        let active = AgentSession::new("active-1", "a", "A");
        let mut ended = AgentSession::new("ended-1", "b", "B");
        ended.phase = Phase::Ended;
        ended.ended_at = Some(Utc::now());

        store.save(&active).unwrap();
        store.save(&ended).unwrap();

        let ended_sessions = store.find_ended().unwrap();
        assert_eq!(ended_sessions.len(), 1);
        assert_eq!(ended_sessions[0].session_id, "ended-1");
    }

    // SessionStore: count

    #[test]
    fn test_store_count() {
        let (_dir, store) = make_store();

        assert_eq!(store.count().unwrap(), 0);

        store.save(&make_session()).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        store.save(&AgentSession::new("sess-2", "a", "A")).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    // SessionStore: for_repo

    #[test]
    fn test_store_for_repo() {
        let dir = TempDir::new().unwrap();
        let store = SessionStore::for_repo(dir.path()).unwrap();

        // Should have created .atomic/sessions/
        assert!(dir.path().join(".atomic").join("sessions").is_dir());

        // Should work for save/load
        store.save(&make_session()).unwrap();
        let loaded = store.load("sess-abc-123").unwrap();
        assert!(loaded.is_some());
    }

    // SessionStore: debug

    #[test]
    fn test_store_debug() {
        let (_dir, store) = make_store();
        let debug = format!("{:?}", store);
        assert!(debug.contains("SessionStore"));
        assert!(debug.contains("sessions_dir"));
    }

    // Full lifecycle integration

    #[test]
    fn test_full_lifecycle() {
        let (_dir, store) = make_store();

        // Create session
        let mut session = AgentSession::new("lifecycle-test", "claude-code", "Claude Code");
        session.set_model_info("anthropic", "claude-sonnet-4");
        store.save(&session).unwrap();

        // Turn 1
        session.begin_turn();
        session.phase = Phase::Active;
        store.save(&session).unwrap();

        let turn_num = session.end_turn();
        assert_eq!(turn_num, 1);
        session.phase = Phase::Idle;
        session.add_files_touched(&["src/main.rs".to_string()]);
        store.save(&session).unwrap();

        // Turn 2
        session.begin_turn();
        session.phase = Phase::Active;
        let turn_num = session.end_turn();
        assert_eq!(turn_num, 2);
        session.phase = Phase::Idle;
        session.add_files_touched(&["src/lib.rs".to_string()]);
        store.save(&session).unwrap();

        // End session
        session.phase = Phase::Ended;
        session.ended_at = Some(Utc::now());
        store.save(&session).unwrap();

        // Verify final state
        let loaded = store.load("lifecycle-test").unwrap().unwrap();
        assert_eq!(loaded.turn_count, 2);
        assert_eq!(loaded.phase, Phase::Ended);
        assert_eq!(loaded.files_touched, vec!["src/main.rs", "src/lib.rs"]);
        assert!(loaded.ended_at.is_some());
        assert_eq!(loaded.agent_vendor, "anthropic");
        assert_eq!(loaded.model, "claude-sonnet-4");

        // Verify it shows up in find_ended
        let ended = store.find_ended().unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].session_id, "lifecycle-test");
    }
}

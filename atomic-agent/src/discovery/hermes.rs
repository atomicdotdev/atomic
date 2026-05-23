//! Hermes discovery adapter.
//!
//! Reads conversation history from a Hermes agent's local SQLite store
//! at `~/.hermes/state.db`. Schema verified against NousResearch/hermes-agent
//! (see `hermes_state.py`, schema version 6).
//!
//! # Schema (quoted from upstream `hermes_state.py`)
//!
//! ```sql
//! CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, ...,
//!     started_at REAL NOT NULL, ended_at REAL, ...,  title TEXT, ...);
//! CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
//!     session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT,
//!     tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
//!     timestamp REAL NOT NULL, ...,
//!     reasoning TEXT, reasoning_details TEXT, codex_reasoning_items TEXT);
//! ```
//!
//! Tool calls are embedded in `messages.tool_calls` as a JSON array. Tool
//! results arrive as separate rows with `role = 'tool'`, populating
//! `tool_call_id`, `tool_name`, and `content`. There is no `part` table.
//!
//! Top-level sessions are filtered with `parent_session_id IS NULL`
//! (subagent runs / compression continuations are children and are
//! excluded from default discovery, matching Hermes' own
//! `list_sessions_rich`).
//!
//! Hermes deduplicates messages at write time via `_last_flushed_db_idx`,
//! so the database itself does not contain consecutive duplicates under
//! normal operation. This adapter still applies a defensive consecutive-
//! equivalence pass on read to tolerate hand-edited or replayed DBs.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;

use crate::discovery::reader::open_sqlite_readonly;
use crate::discovery::types::{DiscoveredEvent, DiscoveredEventType, DiscoveredTrace, StorageKind};
use crate::discovery::TraceDiscovery;
use crate::error::{AgentError, AgentResult};

/// Agent identifier used in registry lookups.
pub const HERMES_AGENT_ID: &str = "hermes";

/// Name of the SQLite database file inside the Hermes root directory.
const HERMES_DB_FILENAME: &str = "state.db";

// HermesDiscovery

/// Discovery adapter for the Hermes AI agent.
///
/// Reads from the SQLite database at `<root>/state.db` (default root:
/// `~/.hermes`). Implements [`TraceDiscovery`] and produces normalized
/// [`DiscoveredTrace`] and [`DiscoveredEvent`] values.
///
/// # Thread safety
///
/// This struct is `Send + Sync`. Database connections are opened fresh in each
/// method call — no `Connection` is stored in the struct (per the contract in
/// [`crate::discovery::reader::open_sqlite_readonly`]).
#[derive(Debug, Clone)]
pub struct HermesDiscovery {
    /// Directory containing `state.db` (typically `~/.hermes`).
    root: PathBuf,
}

impl HermesDiscovery {
    /// Construct an adapter rooted at the default Hermes path (`~/.hermes`).
    ///
    /// If the home directory cannot be resolved, falls back to a relative
    /// `.hermes/` path; `is_available()` will report `false` until the path
    /// is populated.
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|home| home.join(".hermes"))
            .unwrap_or_else(|| PathBuf::from(".hermes"));
        Self { root }
    }

    /// Construct an adapter against a custom root directory.
    ///
    /// Used for tests and for alternative install locations. The path need not
    /// exist at construction time; [`TraceDiscovery::is_available`] checks
    /// existence lazily.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the expected path to the Hermes SQLite database.
    fn db_path(&self) -> PathBuf {
        self.root.join(HERMES_DB_FILENAME)
    }
}

impl Default for HermesDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

// TraceDiscovery impl

impl TraceDiscovery for HermesDiscovery {
    fn agent_id(&self) -> &str {
        HERMES_AGENT_ID
    }

    fn display_name(&self) -> &str {
        "Hermes"
    }

    /// Returns `true` when both the root directory and the `state.db` file
    /// exist. Does not open the database.
    fn is_available(&self) -> bool {
        self.root.is_dir() && self.db_path().is_file()
    }

    fn storage_kind(&self) -> StorageKind {
        StorageKind::Sqlite
    }

    /// List all top-level Hermes sessions (those with `parent_session_id IS NULL`),
    /// ordered most-recent first by last message timestamp.
    ///
    /// Child sessions (subagent runs, compression continuations) are excluded,
    /// matching the behaviour of Hermes' own `list_sessions_rich` function.
    fn list_traces(&self) -> AgentResult<Vec<DiscoveredTrace>> {
        let db = self.db_path();
        let conn = open_sqlite_readonly(&db)?;

        let mut stmt = conn
            .prepare(
                "SELECT \
                    s.id           AS session_id, \
                    s.title        AS title, \
                    s.source       AS source, \
                    s.model        AS model, \
                    s.started_at   AS started_at, \
                    COALESCE( \
                        (SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id), \
                        s.started_at \
                    ) AS last_ts, \
                    (SELECT SUBSTR(REPLACE(REPLACE(m.content, X'0A', ' '), X'0D', ' '), 1, 120) \
                       FROM messages m \
                      WHERE m.session_id = s.id \
                        AND m.role = 'user' \
                        AND m.content IS NOT NULL \
                      ORDER BY m.timestamp, m.id \
                      LIMIT 1) AS preview \
                FROM sessions s \
                WHERE s.parent_session_id IS NULL \
                ORDER BY last_ts DESC",
            )
            .map_err(|e| AgentError::DiscoveryReadFailed {
                path: db.clone(),
                reason: e.to_string(),
            })?;

        let traces: Vec<DiscoveredTrace> = stmt
            .query_map([], |row| {
                let session_id: String = row.get(0)?;
                let title: Option<String> = row.get(1)?;
                let last_ts: f64 = row.get(5)?;
                let preview: Option<String> = row.get(6)?;
                Ok((session_id, title, last_ts, preview))
            })
            .map_err(|e| AgentError::DiscoveryReadFailed {
                path: db.clone(),
                reason: e.to_string(),
            })?
            .map(|row| {
                let (session_id, title, last_ts, preview) =
                    row.map_err(|e| AgentError::DiscoveryReadFailed {
                        path: db.clone(),
                        reason: e.to_string(),
                    })?;

                let timestamp = seconds_to_datetime(last_ts);

                // Treat NULL or empty title as absent.
                let title = title.filter(|t| !t.is_empty());
                let preview = preview
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());

                Ok(DiscoveredTrace {
                    trace_id: session_id,
                    agent_id: HERMES_AGENT_ID.to_string(),
                    title,
                    preview,
                    timestamp,
                    directory: None,
                    source_path: db.clone(),
                })
            })
            .collect::<AgentResult<Vec<_>>>()?;

        Ok(traces)
    }

    /// Read all events for a single Hermes session, in chronological order.
    ///
    /// Wraps both reads in `BEGIN DEFERRED ... ROLLBACK` for WAL snapshot
    /// consistency. Applies a defensive consecutive-equivalence dedup pass
    /// after collecting all events (Hermes already deduplicates on write via
    /// `_last_flushed_db_idx`; the pass here guards against hand-edited or
    /// replayed databases).
    fn read_events(&self, trace_id: &str) -> AgentResult<Vec<DiscoveredEvent>> {
        let db = self.db_path();
        let conn = open_sqlite_readonly(&db)?;

        // Lock a read snapshot so queries see the same WAL state.
        conn.execute_batch("BEGIN DEFERRED")
            .map_err(|e| AgentError::DiscoveryReadFailed {
                path: db.clone(),
                reason: e.to_string(),
            })?;

        let result = read_session_events(&conn, &db, trace_id);

        // Always end the transaction — read-only, so rollback to release.
        let _ = conn.execute_batch("ROLLBACK");

        result
    }
}

// Private helpers

/// Read and assemble all events for `session_id` from the `messages` table.
fn read_session_events(
    conn: &Connection,
    db: &Path,
    session_id: &str,
) -> AgentResult<Vec<DiscoveredEvent>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, \
                    timestamp, reasoning, reasoning_details, finish_reason \
             FROM messages \
             WHERE session_id = ?1 \
             ORDER BY timestamp, id",
        )
        .map_err(|e| AgentError::DiscoveryReadFailed {
            path: db.to_path_buf(),
            reason: e.to_string(),
        })?;

    let rows = stmt
        .query_map([session_id], |row| {
            let id: i64 = row.get(0)?;
            let role: String = row.get(1)?;
            let content: Option<String> = row.get(2)?;
            let tool_call_id: Option<String> = row.get(3)?;
            let tool_calls: Option<String> = row.get(4)?;
            let tool_name: Option<String> = row.get(5)?;
            let timestamp: f64 = row.get(6)?;
            let reasoning: Option<String> = row.get(7)?;
            let reasoning_details: Option<String> = row.get(8)?;
            let finish_reason: Option<String> = row.get(9)?;
            Ok((
                id,
                role,
                content,
                tool_call_id,
                tool_calls,
                tool_name,
                timestamp,
                reasoning,
                reasoning_details,
                finish_reason,
            ))
        })
        .map_err(|e| AgentError::DiscoveryReadFailed {
            path: db.to_path_buf(),
            reason: e.to_string(),
        })?;

    let mut events: Vec<DiscoveredEvent> = Vec::new();

    for row_result in rows {
        let (
            id,
            role,
            content,
            tool_call_id,
            tool_calls,
            tool_name,
            timestamp_secs,
            reasoning,
            reasoning_details,
            finish_reason,
        ) = row_result.map_err(|e| AgentError::DiscoveryReadFailed {
            path: db.to_path_buf(),
            reason: e.to_string(),
        })?;

        let ts = Some(seconds_to_datetime(timestamp_secs));

        // Build the raw_json snapshot of the source row.
        let row_raw = json!({
            "source": "messages",
            "id": id,
            "role": role,
            "content": content,
            "tool_call_id": tool_call_id,
            "tool_calls": tool_calls,
            "tool_name": tool_name,
            "timestamp": timestamp_secs,
            "reasoning": reasoning,
            "reasoning_details": reasoning_details,
            "finish_reason": finish_reason,
        });

        // 1. Emit AssistantThinking if reasoning is present.
        //    Prefer reasoning_details if both are set.
        let thinking_text = match (&reasoning_details, &reasoning) {
            (Some(rd), _) if !rd.is_empty() => Some(rd.clone()),
            (None, Some(r)) if !r.is_empty() => Some(r.clone()),
            _ => None,
        };
        if let Some(thinking) = thinking_text {
            events.push(DiscoveredEvent {
                event_type: DiscoveredEventType::AssistantThinking,
                role: Some("assistant".to_string()),
                text: Some(thinking),
                tool_name: None,
                tool_call_id: None,
                model_id: None,
                timestamp: ts,
                order: 0,
                raw_json: row_raw.clone(),
            });
        }

        // 2. Emit the role-derived event.
        match role.as_str() {
            "user" => {
                events.push(DiscoveredEvent {
                    event_type: DiscoveredEventType::UserMessage,
                    role: Some("user".to_string()),
                    text: content.clone(),
                    tool_name: None,
                    tool_call_id: None,
                    model_id: None,
                    timestamp: ts,
                    order: 0,
                    raw_json: row_raw.clone(),
                });
            }
            "assistant" => {
                // Omit AssistantText when content is absent AND tool_calls is present.
                let has_tool_calls = tool_calls
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                let has_content = content.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
                if has_content || !has_tool_calls {
                    events.push(DiscoveredEvent {
                        event_type: DiscoveredEventType::AssistantText,
                        role: Some("assistant".to_string()),
                        text: content.clone(),
                        tool_name: None,
                        tool_call_id: None,
                        model_id: None,
                        timestamp: ts,
                        order: 0,
                        raw_json: row_raw.clone(),
                    });
                }
            }
            "tool" => {
                events.push(DiscoveredEvent {
                    event_type: DiscoveredEventType::ToolResult,
                    role: Some("tool".to_string()),
                    text: content.clone(),
                    tool_name: tool_name.clone(),
                    tool_call_id: tool_call_id.clone(),
                    model_id: None,
                    timestamp: ts,
                    order: 0,
                    raw_json: row_raw.clone(),
                });
            }
            "system" => {
                // Documented limitation: DiscoveredEventType has no SystemPrompt variant.
                // Map to UserMessage but preserve role for downstream filters.
                events.push(DiscoveredEvent {
                    event_type: DiscoveredEventType::UserMessage,
                    role: Some("system".to_string()),
                    text: content.clone(),
                    tool_name: None,
                    tool_call_id: None,
                    model_id: None,
                    timestamp: ts,
                    order: 0,
                    raw_json: row_raw.clone(),
                });
            }
            other => {
                log::warn!(
                    "hermes discovery: unknown message role '{}' in session {}, emitting Error event",
                    other,
                    session_id
                );
                events.push(DiscoveredEvent {
                    event_type: DiscoveredEventType::Error,
                    role: Some(other.to_string()),
                    text: Some(format!("unknown role: {}", other)),
                    tool_name: None,
                    tool_call_id: None,
                    model_id: None,
                    timestamp: ts,
                    order: 0,
                    raw_json: row_raw.clone(),
                });
            }
        }

        // 3. Parse tool_calls JSON and emit ToolCall events.
        if let Some(ref tc_json) = tool_calls {
            let trimmed = tc_json.trim();
            if !trimmed.is_empty() {
                match serde_json::from_str::<Vec<HermesToolCall>>(trimmed) {
                    Ok(calls) => {
                        for call in calls {
                            let call_id = call.id.clone();
                            let fn_name = call.function.as_ref().and_then(|f| f.name.clone());
                            let fn_args = call.function.as_ref().and_then(|f| f.arguments.clone());

                            let raw_call = json!({
                                "source": "tool_calls",
                                "id": call_id,
                                "function": {
                                    "name": fn_name,
                                    "arguments": fn_args,
                                },
                            });

                            events.push(DiscoveredEvent {
                                event_type: DiscoveredEventType::ToolCall,
                                role: Some("assistant".to_string()),
                                text: fn_args,
                                tool_name: fn_name,
                                tool_call_id: call_id,
                                model_id: None,
                                timestamp: ts,
                                order: 0,
                                raw_json: raw_call,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "hermes discovery: malformed tool_calls JSON in session {} row {}: {}",
                            session_id,
                            id,
                            e
                        );
                        events.push(DiscoveredEvent {
                            event_type: DiscoveredEventType::Error,
                            role: None,
                            text: Some(format!("malformed tool_calls JSON: {}", e)),
                            tool_name: None,
                            tool_call_id: None,
                            model_id: None,
                            timestamp: ts,
                            order: 0,
                            raw_json: row_raw.clone(),
                        });
                    }
                }
            }
        }
    }

    // Defensive dedup pass (Hermes already deduplicates on write).
    let events = dedupe_consecutive(events);

    // Assign monotonic order after dedup.
    let events: Vec<DiscoveredEvent> = events
        .into_iter()
        .enumerate()
        .map(|(i, mut ev)| {
            ev.order = i as u64;
            ev
        })
        .collect();

    Ok(events)
}

/// Convert a Hermes `REAL` timestamp (seconds since Unix epoch, possibly
/// fractional) to a [`DateTime<Utc>`].
///
/// Non-finite or negative values produce an epoch-sentinel timestamp (1970-01-01
/// 00:00:00 UTC) and a `log::warn!`. Timestamps outside the `DateTime` range
/// also fall back to the sentinel.
fn seconds_to_datetime(secs: f64) -> DateTime<Utc> {
    if !secs.is_finite() || secs < 0.0 {
        log::warn!(
            "hermes: non-finite or negative timestamp ({}), using epoch sentinel",
            secs
        );
        return DateTime::<Utc>::from_timestamp(0, 0).expect("epoch must be valid");
    }
    let whole = secs.trunc() as i64;
    let frac_nanos = (secs.fract() * 1_000_000_000.0).round().max(0.0) as u32;
    DateTime::<Utc>::from_timestamp(whole, frac_nanos.min(999_999_999)).unwrap_or_else(|| {
        log::warn!(
            "hermes: timestamp {} out of DateTime range, using epoch sentinel",
            secs
        );
        DateTime::<Utc>::from_timestamp(0, 0).expect("epoch must be valid")
    })
}

/// OpenAI/OpenRouter-compatible tool-call entry stored in `messages.tool_calls`.
///
/// All fields are optional to tolerate null or missing fields in hand-edited or
/// future-schema databases.
#[derive(Debug, Deserialize)]
struct HermesToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<HermesToolFunction>,
}

/// The `function` sub-object of a [`HermesToolCall`].
#[derive(Debug, Deserialize)]
struct HermesToolFunction {
    #[serde(default)]
    name: Option<String>,
    /// Arguments stored as a JSON string per OpenAI/OpenRouter convention.
    #[serde(default)]
    arguments: Option<String>,
}

/// Returns `true` when two events carry equivalent conversational content.
///
/// Used to deduplicate consecutive identical events that may appear in
/// hand-edited or replayed databases. Compares event type, role, text,
/// tool name, and tool call id. Hermes normally prevents duplicates at
/// write time via `_last_flushed_db_idx`; this is a defensive read-side pass.
fn are_hermes_messages_equivalent(a: &DiscoveredEvent, b: &DiscoveredEvent) -> bool {
    a.event_type == b.event_type
        && a.role == b.role
        && a.text == b.text
        && a.tool_name == b.tool_name
        && a.tool_call_id == b.tool_call_id
}

/// Remove consecutive duplicate events from `events`.
fn dedupe_consecutive(events: Vec<DiscoveredEvent>) -> Vec<DiscoveredEvent> {
    let mut out: Vec<DiscoveredEvent> = Vec::with_capacity(events.len());
    for ev in events {
        if let Some(prev) = out.last() {
            if are_hermes_messages_equivalent(prev, &ev) {
                continue;
            }
        }
        out.push(ev);
    }
    out
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Create the canonical Hermes schema in `conn` and return it for seeding.
    fn make_fixture(root: &Path) -> Connection {
        std::fs::create_dir_all(root).unwrap();
        let db_path = root.join(HERMES_DB_FILENAME);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id                  TEXT PRIMARY KEY,
                source              TEXT NOT NULL,
                user_id             TEXT,
                model               TEXT,
                model_config        TEXT,
                system_prompt       TEXT,
                parent_session_id   TEXT,
                started_at          REAL NOT NULL,
                ended_at            REAL,
                end_reason          TEXT,
                message_count       INTEGER DEFAULT 0,
                tool_call_count     INTEGER DEFAULT 0,
                input_tokens        INTEGER DEFAULT 0,
                output_tokens       INTEGER DEFAULT 0,
                cache_read_tokens   INTEGER DEFAULT 0,
                cache_write_tokens  INTEGER DEFAULT 0,
                reasoning_tokens    INTEGER DEFAULT 0,
                billing_provider    TEXT,
                billing_base_url    TEXT,
                billing_mode        TEXT,
                estimated_cost_usd  REAL,
                actual_cost_usd     REAL,
                cost_status         TEXT,
                cost_source         TEXT,
                pricing_version     TEXT,
                title               TEXT,
                FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
            );
            CREATE TABLE messages (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id              TEXT NOT NULL REFERENCES sessions(id),
                role                    TEXT NOT NULL,
                content                 TEXT,
                tool_call_id            TEXT,
                tool_calls              TEXT,
                tool_name               TEXT,
                timestamp               REAL NOT NULL,
                token_count             INTEGER,
                finish_reason           TEXT,
                reasoning               TEXT,
                reasoning_details       TEXT,
                codex_reasoning_items   TEXT
            );
            CREATE INDEX idx_messages_session ON messages(session_id, timestamp);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_agent_id_and_display_name() {
        let dir = tempdir().unwrap();
        let adapter = HermesDiscovery::with_root(dir.path());
        assert_eq!(adapter.agent_id(), "hermes");
        assert_eq!(adapter.display_name(), "Hermes");
    }

    #[test]
    fn test_storage_kind_is_sqlite() {
        let dir = tempdir().unwrap();
        let adapter = HermesDiscovery::with_root(dir.path());
        assert_eq!(adapter.storage_kind(), StorageKind::Sqlite);
    }

    #[test]
    fn test_is_available_missing_root() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent");
        let adapter = HermesDiscovery::with_root(missing);
        assert!(!adapter.is_available());
    }

    #[test]
    fn test_is_available_root_without_db() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        let adapter = HermesDiscovery::with_root(dir.path());
        assert!(!adapter.is_available());
    }

    #[test]
    fn test_is_available_with_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(HERMES_DB_FILENAME);
        std::fs::write(&db_path, b"").unwrap();
        let adapter = HermesDiscovery::with_root(dir.path());
        assert!(adapter.is_available());
    }

    #[test]
    fn test_list_traces_empty_db_no_sessions() {
        let dir = tempdir().unwrap();
        make_fixture(dir.path());
        let adapter = HermesDiscovery::with_root(dir.path());
        let traces = adapter.list_traces().unwrap();
        assert!(traces.is_empty());
    }

    #[test]
    fn test_list_traces_excludes_child_sessions() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        // Parent session
        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('parent-1', 'cli', 100.0)",
            [],
        )
        .unwrap();

        // Child session with parent_session_id set
        conn.execute(
            "INSERT INTO sessions (id, source, started_at, parent_session_id) VALUES ('child-1', 'cli', 200.0, 'parent-1')",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let traces = adapter.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trace_id, "parent-1");
    }

    #[test]
    fn test_list_traces_uses_session_title_when_present() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at, title) VALUES ('sess-title', 'cli', 100.0, 'Refactor auth')",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let traces = adapter.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].title.as_deref(), Some("Refactor auth"));
    }

    #[test]
    fn test_list_traces_preview_is_first_user_message() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-p', 'cli', 1.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('sess-p', 'user', 'Open the PR description', 1.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('sess-p', 'assistant', 'OK', 2.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let traces = adapter.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(
            traces[0].preview.as_deref(),
            Some("Open the PR description")
        );
    }

    #[test]
    fn test_list_traces_timestamp_uses_max_message_timestamp() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-ts', 'cli', 100.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('sess-ts', 'user', 'hi', 200.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let traces = adapter.list_traces().unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].timestamp.timestamp(), 200);
    }

    #[test]
    fn test_read_events_returns_messages_in_order() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-x', 'cli', 1.0)",
            [],
        )
        .unwrap();

        for (ts, role, text) in [
            (1.0f64, "user", "first message"),
            (2.0f64, "assistant", "second message"),
            (3.0f64, "user", "third message"),
        ] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["sess-x", role, text, ts],
            )
            .unwrap();
        }

        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("sess-x").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].order, 0);
        assert_eq!(events[1].order, 1);
        assert_eq!(events[2].order, 2);
        assert_eq!(events[0].event_type, DiscoveredEventType::UserMessage);
        assert_eq!(events[1].event_type, DiscoveredEventType::AssistantText);
        assert_eq!(events[2].event_type, DiscoveredEventType::UserMessage);
        assert_eq!(events[0].text.as_deref(), Some("first message"));
        assert_eq!(events[1].text.as_deref(), Some("second message"));
        assert_eq!(events[2].text.as_deref(), Some("third message"));
    }

    #[test]
    fn test_read_events_emits_tool_calls_from_json() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-tc', 'cli', 1.0)",
            [],
        )
        .unwrap();

        // Assistant message with no content text but with tool_calls JSON.
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
             VALUES ('sess-tc', 'assistant', NULL, '[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]', 1.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("sess-tc").unwrap();

        // Content is NULL and tool_calls is present: no AssistantText, just ToolCall.
        let tool_calls: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == DiscoveredEventType::ToolCall)
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].tool_name.as_deref(), Some("read"));
        assert_eq!(tool_calls[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_calls[0].text.as_deref(), Some("{}"));
    }

    #[test]
    fn test_read_events_tool_role_emits_tool_result() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-tr', 'cli', 1.0)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp) \
             VALUES ('sess-tr', 'tool', 'ok', 'call_1', 'read', 1.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("sess-tr").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, DiscoveredEventType::ToolResult);
        assert_eq!(events[0].tool_name.as_deref(), Some("read"));
        assert_eq!(events[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(events[0].text.as_deref(), Some("ok"));
    }

    #[test]
    fn test_read_events_emits_thinking_from_reasoning_column() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-think', 'cli', 1.0)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning, timestamp) \
             VALUES ('sess-think', 'assistant', 'answer', 'thinking out loud...', 1.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("sess-think").unwrap();

        // Expect: AssistantThinking BEFORE AssistantText
        assert!(events.len() >= 2);
        assert_eq!(events[0].event_type, DiscoveredEventType::AssistantThinking);
        assert_eq!(events[0].text.as_deref(), Some("thinking out loud..."));
        assert_eq!(events[1].event_type, DiscoveredEventType::AssistantText);
    }

    #[test]
    fn test_read_events_unknown_session_returns_empty() {
        let dir = tempdir().unwrap();
        make_fixture(dir.path());
        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("no-such-session").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_read_events_dedupes_consecutive_identical_messages() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-d', 'cli', 1.0)",
            [],
        )
        .unwrap();

        // Two adjacent rows with identical role/content at different timestamps.
        for ts in [1.0f64, 2.0f64] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) \
                 VALUES ('sess-d', 'user', 'duplicate text', ?1)",
                rusqlite::params![ts],
            )
            .unwrap();
        }

        let adapter = HermesDiscovery::with_root(dir.path());
        let events = adapter.read_events("sess-d").unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_read_events_handles_malformed_tool_calls_json() {
        let dir = tempdir().unwrap();
        let conn = make_fixture(dir.path());

        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES ('sess-bad', 'cli', 1.0)",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
             VALUES ('sess-bad', 'assistant', NULL, '{not json', 1.0)",
            [],
        )
        .unwrap();

        let adapter = HermesDiscovery::with_root(dir.path());
        // Must not panic; must return Ok with an Error event.
        let events = adapter.read_events("sess-bad").unwrap();
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == DiscoveredEventType::Error)
            .collect();
        assert!(
            !error_events.is_empty(),
            "expected at least one Error event for malformed tool_calls JSON"
        );
        assert!(error_events[0]
            .text
            .as_deref()
            .unwrap_or("")
            .contains("malformed tool_calls JSON"));
    }
}

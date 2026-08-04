//! OpenCode session recovery from its local SQLite store.
//!
//! OpenCode does not write transcript files — sessions, messages, and
//! parts live in a SQLite database (`opencode.db` in OpenCode's data
//! directory). Its hooks therefore have no `transcript_path` to point at,
//! and thin plugin versions forward only session/model metadata in the
//! stop payload. Without help, an OpenCode turn records with none of:
//!
//! - the unhashed `agent_turn` data (gated on `session.transcript_path`),
//! - reasoning text (only recent plugins forward `reasoning_blocks`),
//! - the agent's response (no plugin puts it in the stop payload).
//!
//! This module reads the session back from OpenCode's own store at turn
//! end and synthesizes what other agents' hooks provide natively: a
//! session transcript (JSONL, one line per content part) plus the current
//! turn's reasoning blocks, response text, token usage, cost, finish
//! reason, and step count. The orchestrator folds them into the session
//! and the stop payload before recording.
//!
//! Everything here is best-effort: OpenCode owns the database and its
//! schema, so every failure is logged and swallowed — the turn records
//! with whatever the plugin sent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

/// Name of the OpenCode SQLite database inside its data directory.
const DB_FILENAME: &str = "opencode.db";

/// How long to wait on a database lock before giving up. OpenCode runs
/// WAL in practice, where readers do not block on the writer; the cap
/// guarantees the agent's stop hook is never stalled by this read.
const DB_BUSY_TIMEOUT_MS: u64 = 100;

/// A reasoning block recovered for the current turn.
#[derive(Debug)]
pub(crate) struct ReasoningBlock {
    pub text: String,
    pub duration_ms: Option<u64>,
}

/// A tool part recovered from the store, keyed by the same call id the
/// plugin puts in the hook payload (`tool_call_id` on graph nodes).
#[derive(Debug)]
pub(crate) struct ToolPart {
    pub call_id: String,
    pub tool: String,
    pub input: Option<Value>,
    pub output: Option<String>,
    pub status: Option<String>,
}

/// Turn data recovered from OpenCode's SQLite store.
#[derive(Debug)]

pub(crate) struct TurnData {
    /// JSONL transcript of the whole session, one line per content part.
    pub transcript_jsonl: String,
    /// Reasoning blocks belonging to the current turn.
    pub reasoning_blocks: Vec<ReasoningBlock>,
    /// The last assistant text part of the current turn.
    pub response: Option<String>,
    /// Token usage summed over the current turn's steps.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// USD cost summed over the current turn's steps.
    pub cost_usd: f64,
    /// Finish reason of the turn's last step ("stop", "tool-calls",
    /// "length").
    pub finish_reason: Option<String>,
    /// Number of model steps in the current turn.
    pub step_count: u32,
    /// Tool parts for the session, so graph tool nodes recorded from thin
    /// payloads can be enriched with their commands, files and outputs.
    pub tool_parts: Vec<ToolPart>,
}

/// Locate the OpenCode database, if present.
///
/// Checks `$OPENCODE_HOME`, then `$XDG_DATA_HOME/opencode`, then
/// `~/.local/share/opencode` (OpenCode's default data directory on both
/// Linux and macOS), then the platform data dir as a last resort.
fn locate_db() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("OPENCODE_HOME") {
        candidates.push(PathBuf::from(home));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg).join("opencode"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local").join("share").join("opencode"));
    }
    if let Some(data) = dirs::data_dir() {
        candidates.push(data.join("opencode"));
    }

    candidates
        .into_iter()
        .map(|dir| dir.join(DB_FILENAME))
        .find(|path| path.is_file())
}

/// Read the current turn's data for `session_id` from OpenCode's store.
///
/// Returns `None` when the database is missing or unreadable, the session
/// is unknown, or the session belongs to a different directory than
/// `expected_dir` (a guard against reading an unrelated project's session
/// with a colliding ID).
pub(crate) fn read_turn(session_id: &str, expected_dir: &Path) -> Option<TurnData> {
    let db_path = locate_db()?;

    let conn = match Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("opencode store: cannot open {}: {}", db_path.display(), e);
            return None;
        }
    };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(DB_BUSY_TIMEOUT_MS));

    // The session row carries the project directory; refuse to read a
    // session that belongs to a different checkout.
    let directory: String = match conn.query_row(
        "SELECT directory FROM session WHERE id = ?1",
        [session_id],
        |row| row.get(0),
    ) {
        Ok(d) => d,
        Err(e) => {
            log::debug!(
                "opencode store: session {} not found in {}: {}",
                session_id,
                db_path.display(),
                e
            );
            return None;
        }
    };
    if !same_dir(Path::new(&directory), expected_dir) {
        log::warn!(
            "opencode store: session {} belongs to '{}', not '{}' — skipping",
            session_id,
            directory,
            expected_dir.display()
        );
        return None;
    }

    let messages = read_messages(&conn, session_id)?;
    let parts = read_parts(&conn, session_id)?;
    Some(assemble(&messages, &parts))
}

/// Compare two directories, canonicalizing when possible.
fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a);
    let cb = std::fs::canonicalize(b);
    match (ca, cb) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Messages of the session in chronological order: `(id, role)`.
fn read_messages(conn: &Connection, session_id: &str) -> Option<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, json_extract(data, '$.role') FROM message \
             WHERE session_id = ?1 ORDER BY time_created, id",
        )
        .ok()?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;

    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok((id, role)) => out.push((id, role)),
            Err(e) => {
                log::debug!("opencode store: skipping message row: {}", e);
            }
        }
    }
    Some(out)
}

/// Content parts of the session in chronological order:
/// `(message_id, data)`.
fn read_parts(conn: &Connection, session_id: &str) -> Option<Vec<(String, Value)>> {
    let mut stmt = conn
        .prepare(
            "SELECT message_id, data FROM part \
             WHERE session_id = ?1 ORDER BY time_created, id",
        )
        .ok()?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;

    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok((message_id, data)) => match serde_json::from_str::<Value>(&data) {
                Ok(v) => out.push((message_id, v)),
                Err(e) => log::debug!("opencode store: skipping unparseable part: {}", e),
            },
            Err(e) => {
                log::debug!("opencode store: skipping part row: {}", e);
            }
        }
    }
    Some(out)
}

/// Fold messages and parts into [`TurnData`].
///
/// The current turn starts at the last user message; reasoning, response,
/// and step statistics are scoped to that window, while the transcript
/// covers the whole session (mirroring how Claude Code's transcript file
/// is a full-session record).
fn assemble(messages: &[(String, String)], parts: &[(String, Value)]) -> TurnData {
    let role_of: HashMap<&str, &str> = messages
        .iter()
        .map(|(id, role)| (id.as_str(), role.as_str()))
        .collect();

    // Turn window: message indices from the last user message onward.
    let turn_start = messages
        .iter()
        .rposition(|(_, role)| role == "user")
        .unwrap_or(0);
    let in_turn: HashMap<&str, bool> = messages
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i >= turn_start))
        .collect();

    let mut data = TurnData {
        transcript_jsonl: String::new(),
        reasoning_blocks: Vec::new(),
        response: None,
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: 0.0,
        finish_reason: None,
        step_count: 0,
        tool_parts: Vec::new(),
    };

    let push_line = |line: Value, out: &mut TurnData| {
        if let Ok(s) = serde_json::to_string(&line) {
            out.transcript_jsonl.push_str(&s);
            out.transcript_jsonl.push('\n');
        }
    };

    for (message_id, part) in parts {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        let in_turn = in_turn.get(message_id.as_str()).copied().unwrap_or(false);
        let role = role_of.get(message_id.as_str()).copied().unwrap_or("assistant");

        match part_type {
            "text" => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let line = serde_json::json!({ "type": role, "text": text });
                push_line(line, &mut data);
                if in_turn && role == "assistant" {
                    data.response = Some(text.to_string());
                }
            }
            "reasoning" => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let duration_ms = part
                    .get("time")
                    .and_then(|t| t.get("start"))
                    .and_then(Value::as_u64)
                    .zip(part.get("time").and_then(|t| t.get("end")).and_then(Value::as_u64))
                    .map(|(start, end)| end.saturating_sub(start));
                let mut line = serde_json::json!({ "type": "reasoning", "text": text });
                if let Some(ms) = duration_ms {
                    line["duration_ms"] = serde_json::Value::from(ms);
                }
                push_line(line, &mut data);
                if in_turn {
                    data.reasoning_blocks.push(ReasoningBlock {
                        text: text.to_string(),
                        duration_ms,
                    });
                }
            }
            "tool" => {
                let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let state = part.get("state");
                let title = state.and_then(|s| s.get("title")).and_then(Value::as_str);
                let status = state.and_then(|s| s.get("status")).and_then(Value::as_str);
                let mut line = serde_json::json!({ "type": "tool", "tool": tool });
                if let Some(title) = title {
                    line["title"] = serde_json::Value::from(title.to_string());
                }
                if let Some(status) = status {
                    line["status"] = serde_json::Value::from(status.to_string());
                }
                push_line(line, &mut data);
                // Recover the full tool record so graph nodes recorded from
                // thin payloads can be enriched with commands/files/outputs.
                if let Some(call_id) = part.get("callID").and_then(Value::as_str) {
                    let output = state
                        .and_then(|s| s.get("output"))
                        .and_then(Value::as_str)
                        .map(|o| o.chars().take(2_000).collect());
                    data.tool_parts.push(ToolPart {
                        call_id: call_id.to_string(),
                        tool: tool.to_string(),
                        input: state.and_then(|s| s.get("input")).cloned(),
                        output,
                        status: status.map(|s| s.to_string()),
                    });
                }
            }
            "step-start" => {
                if in_turn {
                    data.step_count += 1;
                }
            }
            "step-finish" => {
                if !in_turn {
                    continue;
                }
                if let Some(tokens) = part.get("tokens") {
                    data.input_tokens += tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
                    data.output_tokens +=
                        tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
                    data.reasoning_tokens += tokens
                        .get("reasoning")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if let Some(cache) = tokens.get("cache") {
                        data.cache_read_tokens +=
                            cache.get("read").and_then(Value::as_u64).unwrap_or(0);
                        data.cache_write_tokens +=
                            cache.get("write").and_then(Value::as_u64).unwrap_or(0);
                    }
                }
                data.cost_usd += part.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
                if let Some(reason) = part.get("reason").and_then(Value::as_str) {
                    data.finish_reason = Some(reason.to_string());
                }
            }
            _ => {}
        }
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, role: &str) -> (String, String) {
        (id.to_string(), role.to_string())
    }

    fn part(message_id: &str, data: Value) -> (String, Value) {
        (message_id.to_string(), data)
    }

    #[test]
    fn turn_window_scopes_reasoning_and_response() {
        let messages = vec![msg("m1", "user"), msg("m2", "assistant"), msg("m3", "user"), msg("m4", "assistant")];
        let parts = vec![
            part("m1", serde_json::json!({"type": "text", "text": "first question"})),
            part("m2", serde_json::json!({"type": "reasoning", "text": "old thinking"})),
            part("m2", serde_json::json!({"type": "text", "text": "old answer"})),
            part("m3", serde_json::json!({"type": "text", "text": "second question"})),
            part("m4", serde_json::json!({"type": "reasoning", "text": "new thinking", "time": {"start": 10, "end": 52}})),
            part("m4", serde_json::json!({"type": "text", "text": "new answer"})),
        ];

        let data = assemble(&messages, &parts);

        // Response and reasoning come from the last turn only.
        assert_eq!(data.response.as_deref(), Some("new answer"));
        assert_eq!(data.reasoning_blocks.len(), 1);
        assert_eq!(data.reasoning_blocks[0].text, "new thinking");
        assert_eq!(data.reasoning_blocks[0].duration_ms, Some(42));

        // Transcript covers the whole session.
        assert_eq!(data.transcript_jsonl.lines().count(), 6);
        assert!(data.transcript_jsonl.contains("first question"));
        assert!(data.transcript_jsonl.contains("old answer"));
    }

    #[test]
    fn step_finish_sums_tokens_cost_and_reason() {
        let messages = vec![msg("m1", "user"), msg("m2", "assistant")];
        let parts = vec![
            part("m2", serde_json::json!({"type": "step-start"})),
            part(
                "m2",
                serde_json::json!({"type": "step-finish", "reason": "stop", "cost": 0.5,
                    "tokens": {"input": 10, "output": 20, "reasoning": 5, "cache": {"read": 3, "write": 2}}}),
            ),
            part("m2", serde_json::json!({"type": "step-start"})),
            part(
                "m2",
                serde_json::json!({"type": "step-finish", "reason": "tool-calls", "cost": 0.25,
                    "tokens": {"input": 1, "output": 2}}),
            ),
        ];

        let data = assemble(&messages, &parts);

        assert_eq!(data.step_count, 2);
        assert_eq!(data.input_tokens, 11);
        assert_eq!(data.output_tokens, 22);
        assert_eq!(data.reasoning_tokens, 5);
        assert_eq!(data.cache_read_tokens, 3);
        assert_eq!(data.cache_write_tokens, 2);
        assert!((data.cost_usd - 0.75).abs() < f64::EPSILON);
        assert_eq!(data.finish_reason.as_deref(), Some("tool-calls"));
    }

    #[test]
    fn empty_parts_produce_empty_turn() {
        let data = assemble(&[], &[]);
        assert!(data.transcript_jsonl.is_empty());
        assert!(data.response.is_none());
        assert!(data.reasoning_blocks.is_empty());
        assert_eq!(data.step_count, 0);
    }
}

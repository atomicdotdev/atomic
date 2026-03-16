//! agent-trace JSONL export.
//!
//! Converts a [`ProvenanceGraph`] to the vendor-neutral `agent-trace` format
//! and appends it to `.agent-trace/traces.jsonl` in the repository root.
//!
//! ## Design
//!
//! The export is **best-effort**: every function that performs I/O swallows
//! errors after logging them so a write failure never blocks the agent session.
//!
//! ## File layout
//!
//! ```text
//! {root}/.agent-trace/traces.jsonl   ← one JSON object per line
//! ```
//!
//! Each line is a self-contained [`TraceRecord`].  Consumers can stream the
//! file with `jq` or load it entirely into memory; the format is compatible
//! with the broader `agent-trace` ecosystem (Cursor, Amp, Cline, OpenCode).
//!
//! ## VCS revision
//!
//! The `vcs` field is populated by opening the Atomic repository at `root` and
//! reading the current stack's Merkle state via
//! [`atomic_repository::Repository::get_stack_info`].  This gives a
//! content-addressed, tamper-evident revision that is native to the Atomic
//! graph model.  Git is not consulted.
//!
//! ## Sherpa extension
//!
//! When `graph.profile == "sherpa-trace/1.0.0"` the record's
//! `metadata["dev.atomic"]` object is populated with:
//!
//! - `profile`       — [`TraceProfile`] discriminator
//! - `intent`        — parsed [`GoalDetail`] from the `Goal` node
//! - `todos`         — list of parsed [`CommitmentDetail`] from `Commitment` nodes
//! - `verification`  — parsed [`VerificationDetail`] from the `Verification` node
//!
//! Non-Sherpa graphs get an empty `metadata["dev.atomic"]` object so consumers
//! can always rely on the key being present.

use std::collections::HashMap;
use std::path::Path;

use atomic_core::change::provenance_graph::{ProvenanceGraph, ProvenanceNodeKind};
use atomic_core::types::Base32;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provenance::detail::{CommitmentDetail, GoalDetail, VerificationDetail};

// ---------------------------------------------------------------------------
// Top-level record type
// ---------------------------------------------------------------------------

/// A single line in `.agent-trace/traces.jsonl`.
///
/// The schema is vendor-neutral; Sherpa-specific data lives under
/// `metadata["dev.atomic"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Format version, e.g. `"0.1.0"`.
    pub version: String,

    /// Unique identifier for this record (UUIDv4 or session-turn composite).
    pub id: String,

    /// ISO 8601 / RFC 3339 timestamp.
    pub timestamp: String,

    /// VCS context (best-effort — absent if not a git repo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs: Option<VcsInfo>,

    /// Tool that produced this record.
    pub tool: ToolInfo,

    /// File-level attribution entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,

    /// Extension metadata.  Always contains at least `"dev.atomic"`.
    pub metadata: HashMap<String, Value>,
}

/// VCS snapshot at record creation time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcsInfo {
    /// Always `"atomic"`.
    #[serde(rename = "type")]
    pub vcs_type: String,

    /// Name of the Atomic stack that was current when the record was written.
    pub stack: String,

    /// Merkle state of the current stack (base32 Blake3 hash of the applied
    /// change sequence) at the time the provenance graph was saved.
    pub revision: String,
}

/// Tool / producer identification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// e.g. `"sherpa"` or `"atomic-agent"`.
    pub name: String,

    /// Crate version string.
    pub version: String,
}

/// One file touched in this turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path from the repository root.
    pub path: String,

    /// Conversations that reference this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversations: Vec<Conversation>,
}

/// A single AI-or-human conversation block that produced changes to a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    /// Canonical URL for this turn, e.g.
    /// `"sherpa://sessions/{session_id}/turns/{turn_id}"`.
    pub url: String,

    /// Who made the changes.
    pub contributor: ContributorInfo,

    /// Line ranges within the file that were affected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<LineRange>,

    /// Cross-references (atomic change hash, todo URL, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedRef>,
}

/// Contributor (AI model or human).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributorInfo {
    /// `"ai"` or `"human"`.
    #[serde(rename = "type")]
    pub contributor_type: String,

    /// Model identifier, e.g. `"anthropic/claude-sonnet-4-6"`.
    /// Present only when `type == "ai"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// A half-open line range within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// A cross-reference to a related artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedRef {
    /// `"atomic_change"`, `"todo"`, `"issue"`, etc.
    #[serde(rename = "type")]
    pub ref_type: String,

    /// URL or identifier.
    pub url: String,
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Convert a [`ProvenanceGraph`] into a [`TraceRecord`] suitable for appending
/// to `.agent-trace/traces.jsonl`.
///
/// `root` is the repository root used for computing relative paths and for
/// resolving the git HEAD revision.
///
/// This function never fails; any missing data is silently omitted.
pub fn to_agent_trace_record(graph: &ProvenanceGraph, root: &Path) -> TraceRecord {
    let timestamp = DateTime::<Utc>::from_timestamp(graph.timestamp, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339();

    // Unique record ID: session + timestamp epoch
    let id = format!("{}-{}", graph.session_id, graph.timestamp);

    // VCS: best-effort Atomic stack Merkle state
    let vcs = atomic_revision(root).map(|(stack, revision)| VcsInfo {
        vcs_type: "atomic".to_string(),
        stack,
        revision,
    });

    // Tool info
    let tool = ToolInfo {
        name: graph.agent_name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    // Build files + metadata["dev.atomic"] by walking nodes
    let (files, dev_atomic) = build_files_and_metadata(graph);

    let mut metadata: HashMap<String, Value> = HashMap::new();
    metadata.insert("dev.atomic".to_string(), dev_atomic);

    TraceRecord {
        version: "0.1.0".to_string(),
        id,
        timestamp,
        vcs,
        tool,
        files,
        metadata,
    }
}

/// Append a [`TraceRecord`] as a single JSON line to
/// `{root}/.agent-trace/traces.jsonl`.
///
/// Creates the directory and file if they don't exist.  All errors are
/// logged and swallowed — never blocks the caller.
pub fn append_agent_trace(record: &TraceRecord, root: &Path) {
    let result = try_append_agent_trace(record, root);
    if let Err(e) = result {
        log::warn!("agent-trace: failed to append record: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Walk the graph nodes and build:
/// - `files` — per-file `FileEntry` list (from `Commitment` nodes)
/// - `dev_atomic` — the Sherpa-specific metadata object
fn build_files_and_metadata(graph: &ProvenanceGraph) -> (Vec<FileEntry>, Value) {
    // Collect nodes by kind
    let goal_node = graph
        .nodes
        .iter()
        .find(|n| n.kind == ProvenanceNodeKind::Goal);

    let commitment_nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == ProvenanceNodeKind::Commitment)
        .collect();

    let execution_nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == ProvenanceNodeKind::Execution)
        .collect();

    let verification_node = graph
        .nodes
        .iter()
        .find(|n| n.kind == ProvenanceNodeKind::Verification);

    // Parse detail JSON strings into typed structs (best-effort — None on any
    // parse error so a malformed node never blocks the export).
    let goal_detail: Option<GoalDetail> = goal_node
        .and_then(|n| n.detail.as_deref())
        .and_then(|s| serde_json::from_str(s).ok());

    let commitment_details: Vec<CommitmentDetail> = commitment_nodes
        .iter()
        .filter_map(|n| n.detail.as_deref())
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();

    let verification_detail: Option<VerificationDetail> = verification_node
        .and_then(|n| n.detail.as_deref())
        .and_then(|s| serde_json::from_str(s).ok());

    // Extract turn/session identifiers from GoalDetail
    let session_id = goal_detail
        .as_ref()
        .map(|g| g.session_id.as_str())
        .unwrap_or(&graph.session_id)
        .to_string();

    let turn_id = goal_detail
        .as_ref()
        .map(|g| g.intent_turn_id as u64)
        .unwrap_or(0);

    let model = goal_detail
        .as_ref()
        .map(|g| g.model.as_str())
        .unwrap_or("")
        .to_string();

    // Build FileEntry list from Commitment nodes
    let turn_url = format!("sherpa://sessions/{}/turns/{}", session_id, turn_id);

    let mut files: Vec<FileEntry> = Vec::new();

    for (node, cd) in commitment_nodes.iter().zip(commitment_details.iter()) {
        let path = if cd.file.is_empty() {
            node.summary.clone()
        } else {
            cd.file.clone()
        };

        let contributor_type = cd.contributor.to_string();

        let ranges: Vec<LineRange> = match (cd.start_line, cd.end_line) {
            (Some(s), Some(e)) => vec![LineRange {
                start_line: s,
                end_line: e,
            }],
            _ => vec![],
        };

        let mut related: Vec<RelatedRef> = Vec::new();
        if !cd.todo_id.is_empty() {
            related.push(RelatedRef {
                ref_type: "todo".to_string(),
                url: format!(
                    "sherpa://sessions/{}/turns/{}/todos/{}",
                    session_id, turn_id, cd.todo_id
                ),
            });
        }
        if let Some(ref hash) = cd.atomic_change_hash {
            related.push(RelatedRef {
                ref_type: "atomic_change".to_string(),
                url: format!("atomic://changes/{}", hash),
            });
        }

        let conversation = Conversation {
            url: turn_url.clone(),
            contributor: ContributorInfo {
                contributor_type,
                model_id: if model.is_empty() {
                    None
                } else {
                    Some(model.clone())
                },
            },
            ranges,
            related,
        };

        // Merge into existing FileEntry for this path, or create a new one
        if let Some(entry) = files.iter_mut().find(|f| f.path == path) {
            entry.conversations.push(conversation);
        } else {
            files.push(FileEntry {
                path,
                conversations: vec![conversation],
            });
        }
    }

    // Build execution list grouped by todo_id for metadata.
    // Execution nodes are still read as raw Value because ExecutionDetail is
    // embedded as-is in the todos array — no field access needed on our side.
    let mut executions_by_todo: HashMap<String, Vec<Value>> = HashMap::new();
    for node in &execution_nodes {
        if let Some(detail_str) = &node.detail {
            if let Ok(detail) = serde_json::from_str::<Value>(detail_str) {
                let todo_id = detail["todo_id"].as_str().unwrap_or("unknown").to_string();
                executions_by_todo.entry(todo_id).or_default().push(detail);
            }
        }
    }

    // Attach executions to commitment details for metadata, serializing the
    // typed CommitmentDetail back to Value so it can be embedded in the JSON.
    let todos_with_executions: Vec<Value> = commitment_details
        .iter()
        .filter_map(|cd| {
            let mut entry = serde_json::to_value(cd).ok()?;
            let execs = executions_by_todo
                .get(&cd.todo_id)
                .cloned()
                .unwrap_or_default();
            if !execs.is_empty() {
                entry["executions"] = Value::Array(execs);
            }
            Some(entry)
        })
        .collect();

    // Build the TraceProfile block (present only for Sherpa graphs)
    let profile_value: Value = if graph
        .profile
        .as_deref()
        .map(|p| p.starts_with("sherpa-trace"))
        .unwrap_or(false)
    {
        let phase = verification_detail
            .as_ref()
            .map(|v| v.outcome.to_string())
            .unwrap_or_else(|| "verification".to_string());

        serde_json::json!({
            "schema": "sherpa-trace",
            "schema_version": "1.0.0",
            "producer": "sherpa",
            "producer_version": env!("CARGO_PKG_VERSION"),
            "session_id": session_id,
            "turn_id": turn_id,
            "phase": phase,
        })
    } else {
        Value::Null
    };

    // Assemble dev.atomic metadata
    let mut dev_atomic = serde_json::json!({});

    if !profile_value.is_null() {
        dev_atomic["profile"] = profile_value;
    }
    if let Some(ref gd) = goal_detail {
        dev_atomic["intent"] = serde_json::to_value(gd).unwrap_or(Value::Null);
    }
    if !todos_with_executions.is_empty() {
        dev_atomic["todos"] = Value::Array(todos_with_executions);
    }
    if let Some(ref vd) = verification_detail {
        dev_atomic["verification"] = serde_json::to_value(vd).unwrap_or(Value::Null);
    }

    (files, dev_atomic)
}

/// Open the Atomic repository at `root` and return `(stack_name, merkle_base32)`
/// for the current stack.
///
/// Returns `None` if the directory is not an Atomic repository or any error
/// occurs — callers treat absence of VCS info as non-fatal.
fn atomic_revision(root: &Path) -> Option<(String, String)> {
    let repo = atomic_repository::Repository::open(root).ok()?;
    let stack_name = repo.current_stack().to_string();
    let info = repo.get_stack_info(&stack_name).ok()?;
    Some((stack_name, info.state.to_base32()))
}

/// Inner fallible implementation of [`append_agent_trace`].
fn try_append_agent_trace(record: &TraceRecord, root: &Path) -> std::io::Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    let dir = root.join(".agent-trace");
    fs::create_dir_all(&dir)?;

    let path = dir.join("traces.jsonl");
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;

    let line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(file, "{}", line)?;

    log::debug!(
        "agent-trace: appended record {} to {}",
        record.id,
        path.display()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::provenance_graph::{
        ProvenanceEdge, ProvenanceEdgeKind, ProvenanceNode, ProvenanceNodeKind,
    };

    fn make_node(
        id: &str,
        kind: ProvenanceNodeKind,
        summary: &str,
        detail: Option<&str>,
    ) -> ProvenanceNode {
        ProvenanceNode {
            id: id.to_string(),
            kind,
            timestamp: 1_700_000_000_000,
            summary: summary.to_string(),
            detail: detail.map(|s| s.to_string()),
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        }
    }

    fn sherpa_graph() -> ProvenanceGraph {
        let goal_detail = serde_json::json!({
            "intent_title": "Build Hello World CLI",
            "intent_description": "A simple CLI that prints Hello World",
            "intent_turn_id": 3,
            "session_id": "tiny-star-8412",
            "phases": {
                "orienting":  { "input": 1000, "output": 200, "cost_usd": 0.006 },
                "executing":  { "input": 5000, "output": 1000, "cost_usd": 0.05 }
            },
            "turn_totals": { "input": 6000, "output": 1200, "total": 7200, "cost_usd": 0.056 },
            "model": "anthropic/claude-sonnet-4-6",
            "context_pct": 3.6
        });

        let commitment_detail = serde_json::json!({
            "todo_id": "t2",
            "todo_content": "Create src/index.ts",
            "priority": "high",
            "contributor": "ai",
            "file": "src/index.ts",
            "start_line": 1,
            "end_line": 12,
            "atomic_change_hash": null,
            "tokens": { "input": 1200, "output": 300, "cost_usd": 0.013 }
        });

        let execution_detail = serde_json::json!({
            "todo_id": "t2",
            "command": "npm install",
            "exit_code": 0,
            "duration_ms": 1234,
            "output_summary": "added 42 packages"
        });

        let verification_detail = serde_json::json!({
            "intent_turn_id": 3,
            "intent_title": "Build Hello World CLI",
            "outcome": "completed",
            "summary": "CLI printed Hello World successfully",
            "turn_cost_usd": 0.056,
            "turn_tokens_total": 7200,
            "learnings": { "repo": [], "workflow": [], "code": [] }
        });

        let nodes = vec![
            make_node(
                "g-tiny-3",
                ProvenanceNodeKind::Goal,
                "Build Hello World CLI",
                Some(&goal_detail.to_string()),
            ),
            make_node(
                "c-tiny-3-0",
                ProvenanceNodeKind::Commitment,
                "wrote src/index.ts",
                Some(&commitment_detail.to_string()),
            ),
            make_node(
                "e-tiny-3-0",
                ProvenanceNodeKind::Execution,
                "npm install",
                Some(&execution_detail.to_string()),
            ),
            make_node(
                "v-tiny-3",
                ProvenanceNodeKind::Verification,
                "turn 3 completed",
                Some(&verification_detail.to_string()),
            ),
        ];

        let edges = vec![
            ProvenanceEdge {
                from: "g-tiny-3".into(),
                to: "c-tiny-3-0".into(),
                kind: ProvenanceEdgeKind::LedTo,
            },
            ProvenanceEdge {
                from: "c-tiny-3-0".into(),
                to: "e-tiny-3-0".into(),
                kind: ProvenanceEdgeKind::LedTo,
            },
            ProvenanceEdge {
                from: "e-tiny-3-0".into(),
                to: "v-tiny-3".into(),
                kind: ProvenanceEdgeKind::LedTo,
            },
        ];

        ProvenanceGraph::builder("tiny-star-8412", "sherpa")
            .agent_display_name("Sherpa")
            .agent_vendor("atomic")
            .profile("sherpa-trace/1.0.0")
            .nodes(nodes)
            .edges(edges)
            .timestamp(1_700_000_000)
            .build()
    }

    #[test]
    fn test_record_has_required_fields() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        assert_eq!(record.version, "0.1.0");
        assert!(!record.id.is_empty());
        assert!(!record.timestamp.is_empty());
        assert_eq!(record.tool.name, "sherpa");
    }

    #[test]
    fn test_record_has_dev_atomic_metadata() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        let dev_atomic = &record.metadata["dev.atomic"];
        assert!(dev_atomic.is_object(), "dev.atomic should be an object");
        assert_eq!(dev_atomic["profile"]["schema"], "sherpa-trace");
        assert_eq!(dev_atomic["profile"]["schema_version"], "1.0.0");
        assert_eq!(dev_atomic["profile"]["session_id"], "tiny-star-8412");
        assert_eq!(dev_atomic["profile"]["turn_id"], 3);
    }

    #[test]
    fn test_record_intent_populated() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        let intent = &record.metadata["dev.atomic"]["intent"];
        assert_eq!(intent["intent_title"], "Build Hello World CLI");
        assert_eq!(intent["intent_turn_id"], 3);
    }

    #[test]
    fn test_record_todos_with_executions() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        let todos = &record.metadata["dev.atomic"]["todos"];
        assert!(todos.is_array());
        let todos_arr = todos.as_array().unwrap();
        assert_eq!(todos_arr.len(), 1);
        assert_eq!(todos_arr[0]["todo_id"], "t2");

        // Execution should be nested under the matching todo
        let execs = &todos_arr[0]["executions"];
        assert!(execs.is_array());
        assert_eq!(execs[0]["command"], "npm install");
        assert_eq!(execs[0]["exit_code"], 0);
    }

    #[test]
    fn test_record_verification_populated() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        let verif = &record.metadata["dev.atomic"]["verification"];
        assert_eq!(verif["outcome"], "completed");
        assert_eq!(verif["intent_turn_id"], 3);
    }

    #[test]
    fn test_record_files_populated() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0].path, "src/index.ts");
        assert_eq!(record.files[0].conversations.len(), 1);

        let conv = &record.files[0].conversations[0];
        assert!(conv.url.contains("tiny-star-8412"));
        assert_eq!(conv.contributor.contributor_type, "ai");
        assert_eq!(conv.ranges[0].start_line, 1);
        assert_eq!(conv.ranges[0].end_line, 12);

        // todo related ref
        assert!(conv.related.iter().any(|r| r.ref_type == "todo"));
    }

    #[test]
    fn test_append_and_read_back() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        append_agent_trace(&record, tmp.path());

        let path = tmp.path().join(".agent-trace").join("traces.jsonl");
        assert!(path.exists());

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: TraceRecord = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(
            parsed.metadata["dev.atomic"]["profile"]["schema"],
            "sherpa-trace"
        );
    }

    #[test]
    fn test_append_multiple_records_produces_valid_jsonl() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();

        for _ in 0..3 {
            let record = to_agent_trace_record(&graph, tmp.path());
            append_agent_trace(&record, tmp.path());
        }

        let path = tmp.path().join(".agent-trace").join("traces.jsonl");
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);

        // Every line must parse as valid JSON
        for line in &lines {
            let v: Value = serde_json::from_str(line).expect("each line must be valid JSON");
            assert_eq!(v["version"], "0.1.0");
        }
    }

    #[test]
    fn test_append_idempotent_on_existing_dir() {
        let graph = sherpa_graph();
        let tmp = tempfile::tempdir().unwrap();

        // Pre-create the directory — should not error
        std::fs::create_dir_all(tmp.path().join(".agent-trace")).unwrap();

        let record = to_agent_trace_record(&graph, tmp.path());
        append_agent_trace(&record, tmp.path());

        let path = tmp.path().join(".agent-trace").join("traces.jsonl");
        assert!(path.exists());
    }

    #[test]
    fn test_non_sherpa_graph_still_has_dev_atomic() {
        // A graph without a profile should still produce a dev.atomic key
        let graph = ProvenanceGraph::builder("sess-1", "claude-code")
            .agent_display_name("Claude Code")
            .timestamp(1_700_000_000)
            .build();

        let tmp = tempfile::tempdir().unwrap();
        let record = to_agent_trace_record(&graph, tmp.path());

        assert!(
            record.metadata.contains_key("dev.atomic"),
            "dev.atomic key must always be present"
        );
        // But no profile sub-key since it's not a sherpa graph
        assert!(
            record.metadata["dev.atomic"]["profile"].is_null()
                || !record.metadata["dev.atomic"]
                    .as_object()
                    .map(|o| o.contains_key("profile"))
                    .unwrap_or(false),
            "non-sherpa graph should not have profile in dev.atomic"
        );
    }

    #[test]
    fn test_atomic_revision_returns_none_outside_repo() {
        // A fresh tempdir is not an Atomic repository
        let tmp = tempfile::tempdir().unwrap();
        let rev = atomic_revision(tmp.path());
        assert!(rev.is_none());
    }
}

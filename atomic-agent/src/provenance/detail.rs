//! Typed JSON payloads for Sherpa provenance graph nodes.
//!
//! Each struct in this module is serialized into `ProvenanceNode::detail` (as a
//! JSON string) when Sherpa writes a `ProvenanceGraph` with
//! `profile == "sherpa-trace/1.0.0"`.
//!
//! ## Node kind → detail struct mapping
//!
//! | `ProvenanceNodeKind` | Detail struct          |
//! |----------------------|------------------------|
//! | `Goal`               | [`GoalDetail`]         |
//! | `Commitment`         | [`CommitmentDetail`]   |
//! | `Execution`          | [`ExecutionDetail`]    |
//! | `Verification`       | [`VerificationDetail`] |
//!
//! ## Ownership rationale
//!
//! These types live in `atomic-agent` (not in `atomic-llm` / `atomic-tui`)
//! because they are part of the shared provenance schema that every consumer
//! of a Sherpa-produced `ProvenanceGraph` must agree on:
//!
//! - `atomic-agent/src/export.rs` deserializes them to build typed
//!   `agent-trace` JSONL records.
//! - `atomic-tui/src/provenance.rs` serializes them when flushing a turn.
//! - Future consumers (API, UI, attestation) import them from here.
//!
//! `atomic-llm/src/trace.rs` re-exports every public item from this module
//! so existing Sherpa TUI import paths compile without change.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Cost helpers (duplicated from atomic-llm to avoid a circular dependency)
// ---------------------------------------------------------------------------

/// Cost in USD per 1 000 000 input tokens.
///
/// Matches `atomic_llm::trace::COST_PER_M_INPUT` — both sides must stay in sync.
pub const COST_PER_M_INPUT: f64 = 5.0;

/// Cost in USD per 1 000 000 output tokens.
///
/// Matches `atomic_llm::trace::COST_PER_M_OUTPUT` — both sides must stay in sync.
pub const COST_PER_M_OUTPUT: f64 = 25.0;

/// Compute the USD cost for a pair of token counts.
///
/// Uses [`COST_PER_M_INPUT`] and [`COST_PER_M_OUTPUT`] so every layer that
/// touches token accounting produces identical values.
///
/// # Example
///
/// ```
/// use atomic_agent::provenance::detail::compute_cost;
/// let cost = compute_cost(16_083, 3_746);
/// let expected = (16_083.0 / 1_000_000.0) * 5.0 + (3_746.0 / 1_000_000.0) * 25.0;
/// assert!((cost - expected).abs() < 1e-9);
/// ```
pub fn compute_cost(input_tokens: u32, output_tokens: u32) -> f64 {
    (input_tokens as f64 / 1_000_000.0) * COST_PER_M_INPUT
        + (output_tokens as f64 / 1_000_000.0) * COST_PER_M_OUTPUT
}

// ---------------------------------------------------------------------------
// Shared sub-structs
// ---------------------------------------------------------------------------

/// Token counts and cost for a single phase or todo slice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseTokens {
    /// Input tokens consumed.
    pub input: u32,
    /// Output tokens produced.
    pub output: u32,
    /// Cost in USD computed from [`COST_PER_M_INPUT`] / [`COST_PER_M_OUTPUT`].
    pub cost_usd: f64,
}

impl PhaseTokens {
    /// Build a `PhaseTokens` from raw counts, computing cost automatically.
    pub fn from_counts(input: u32, output: u32) -> Self {
        Self {
            input,
            output,
            cost_usd: compute_cost(input, output),
        }
    }

    /// Add another `PhaseTokens` into `self` (used for running totals).
    pub fn add(&mut self, other: &PhaseTokens) {
        self.input += other.input;
        self.output += other.output;
        self.cost_usd = compute_cost(self.input, self.output);
    }
}

/// Aggregate token counts across all phases in a turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnTotals {
    /// Total input tokens.
    pub input: u32,
    /// Total output tokens.
    pub output: u32,
    /// `input + output`.
    pub total: u32,
    /// Cost in USD.
    pub cost_usd: f64,
}

impl TurnTotals {
    /// Compute totals from a `phases` map.
    pub fn from_phases(phases: &HashMap<String, PhaseTokens>) -> Self {
        let input: u32 = phases.values().map(|p| p.input).sum();
        let output: u32 = phases.values().map(|p| p.output).sum();
        Self {
            input,
            output,
            total: input + output,
            cost_usd: compute_cost(input, output),
        }
    }
}

// ---------------------------------------------------------------------------
// Learnings
// ---------------------------------------------------------------------------

/// A code-level finding tied to a specific file and line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeFinding {
    /// Relative path to the file, e.g. `"src/index.ts"`.
    pub file: String,
    /// Line number (1-based) relevant to the finding.
    pub line: u32,
    /// One-line description of the finding.
    pub finding: String,
}

/// Structured learnings captured at `Verification` time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Learnings {
    /// Repository-level observations, e.g. build commands, conventions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo: Vec<String>,
    /// Workflow observations, e.g. required tool sequencing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow: Vec<String>,
    /// Code-level findings tied to specific files and lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<CodeFinding>,
}

impl Learnings {
    /// Returns `true` if all fields are empty.
    pub fn is_empty(&self) -> bool {
        self.repo.is_empty() && self.workflow.is_empty() && self.code.is_empty()
    }
}

// ---------------------------------------------------------------------------
// GoalDetail
// ---------------------------------------------------------------------------

/// JSON payload for a `ProvenanceNodeKind::Goal` node in a Sherpa graph.
///
/// Stored in `ProvenanceNode::detail` as serialized JSON.
///
/// # Example
///
/// ```json
/// {
///   "intent_title": "Build TypeScript Hello World CLI",
///   "intent_description": "200–500 word narrative spec...",
///   "intent_turn_id": 3,
///   "phases": {
///     "orienting":  { "input": 1842, "output": 312,  "cost_usd": 0.0012 },
///     "proposing":  { "input": 3201, "output": 891,  "cost_usd": 0.0031 },
///     "executing":  { "input": 8940, "output": 2103, "cost_usd": 0.0089 },
///     "grading":    { "input": 2100, "output": 440,  "cost_usd": 0.0018 }
///   },
///   "turn_totals": { "input": 16083, "output": 3746, "total": 19829, "cost_usd": 0.0150 },
///   "model": "anthropic/claude-sonnet-4-6",
///   "context_pct": 9.9
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalDetail {
    /// One-line intent title chosen by the agent.
    pub intent_title: String,

    /// 200–500 word narrative description of the intent.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intent_description: String,

    /// Monotonically increasing turn index within the session.
    pub intent_turn_id: u32,

    /// Optional external issue reference, e.g. `"GH-42"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_ref: Option<String>,

    /// Human-readable session identifier, e.g. `"tiny-star-8412"`.
    pub session_id: String,

    /// Per-phase token breakdown.  Keys are lowercase phase names:
    /// `"orienting"`, `"proposing"`, `"executing"`, `"grading"`.
    pub phases: HashMap<String, PhaseTokens>,

    /// Aggregate across all phases.
    pub turn_totals: TurnTotals,

    /// Model identifier used for this turn, e.g. `"anthropic/claude-sonnet-4-6"`.
    pub model: String,

    /// Fraction of the model's context window used at peak, as a percentage.
    pub context_pct: f64,
}

impl GoalDetail {
    /// Re-derive `turn_totals` from the current `phases` map in place.
    pub fn recompute_totals(&mut self) {
        self.turn_totals = TurnTotals::from_phases(&self.phases);
    }
}

// ---------------------------------------------------------------------------
// CommitmentDetail
// ---------------------------------------------------------------------------

/// Who produced the file changes for a given todo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Contributor {
    /// All changes written autonomously by the agent.
    #[default]
    Ai,
    /// All changes written by the human in guide mode.
    Human,
    /// Mix of agent and human changes (detected post-hoc via diff; v1 deferred).
    Mixed,
}

impl std::fmt::Display for Contributor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ai => f.write_str("ai"),
            Self::Human => f.write_str("human"),
            Self::Mixed => f.write_str("mixed"),
        }
    }
}

/// JSON payload for a `ProvenanceNodeKind::Commitment` node in a Sherpa graph.
///
/// One node is emitted per todo that produced file changes. Stored in
/// `ProvenanceNode::detail` as serialized JSON.
///
/// # Example
///
/// ```json
/// {
///   "todo_id": "t2",
///   "todo_content": "Install typescript as a dev dependency",
///   "priority": "high",
///   "contributor": "ai",
///   "file": "package.json",
///   "start_line": 1,
///   "end_line": 9,
///   "atomic_change_hash": "<base32>",
///   "tokens": { "input": 1203, "output": 287, "cost_usd": 0.0011 }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitmentDetail {
    /// Stable todo identifier, e.g. `"t2"`.
    pub todo_id: String,

    /// Full text of the todo item.
    pub todo_content: String,

    /// Priority label, e.g. `"high"`, `"medium"`, `"low"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,

    /// Who made the changes for this todo.
    pub contributor: Contributor,

    /// Relative path of the primary file changed, e.g. `"src/index.ts"`.
    pub file: String,

    /// First line of the changed range (1-based, inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,

    /// Last line of the changed range (1-based, inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,

    /// Base32 hash of the corresponding `atomic record` change, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atomic_change_hash: Option<String>,

    /// Token slice attributed to this todo's execution.
    pub tokens: PhaseTokens,
}

// ---------------------------------------------------------------------------
// ExecutionDetail
// ---------------------------------------------------------------------------

/// JSON payload for a `ProvenanceNodeKind::Execution` node in a Sherpa graph.
///
/// One node is emitted per `bash` tool call. Stored in `ProvenanceNode::detail`
/// as serialized JSON.
///
/// # Example
///
/// ```json
/// {
///   "todo_id": "t2",
///   "command": "npm install --save-dev typescript",
///   "exit_code": 0,
///   "duration_ms": 616,
///   "output_summary": "added 1 package, audited 2 packages in 616ms"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionDetail {
    /// ID of the todo that was `InProgress` when this command ran.
    pub todo_id: String,

    /// Full command string passed to the shell.
    pub command: String,

    /// Shell exit code (0 = success).
    pub exit_code: i32,

    /// Wall-clock duration of the command in milliseconds.
    pub duration_ms: u64,

    /// First `n` bytes of combined stdout/stderr, truncated for storage.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_summary: String,
}

// ---------------------------------------------------------------------------
// VerificationDetail
// ---------------------------------------------------------------------------

/// The high-level outcome of a Sherpa turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TurnOutcome {
    /// All todos completed successfully.
    #[default]
    Completed,
    /// Some todos completed, others did not.
    Partial,
    /// The turn failed without completing its goal.
    Failed,
}

impl std::fmt::Display for TurnOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => f.write_str("completed"),
            Self::Partial => f.write_str("partial"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

/// JSON payload for a `ProvenanceNodeKind::Verification` node in a Sherpa graph.
///
/// Written when the state machine reaches `State::Verification`. Stored in
/// `ProvenanceNode::detail` as serialized JSON.
///
/// # Example
///
/// ```json
/// {
///   "intent_turn_id": 3,
///   "intent_title": "Build TypeScript Hello World CLI for exec demo",
///   "outcome": "completed",
///   "summary": "npm start compiled and printed 'Hello World!' — zero exit code",
///   "turn_cost_usd": 0.0150,
///   "turn_tokens_total": 19829,
///   "learnings": { "repo": [...], "workflow": [...], "code": [...] },
///   "provenance_graph_hash": "<base32>"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationDetail {
    /// Turn index of the `Goal` node this verification closes.
    pub intent_turn_id: u32,

    /// Intent title copied from the matching `Goal` node.
    pub intent_title: String,

    /// Whether the turn met its goal.
    pub outcome: TurnOutcome,

    /// One-sentence human-readable description of what happened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,

    /// Total cost for this turn in USD.
    pub turn_cost_usd: f64,

    /// Total tokens consumed this turn (`input + output`).
    pub turn_tokens_total: u32,

    /// Structured learnings captured during verification.
    pub learnings: Learnings,

    /// Base32 hash of the `ProvenanceGraph` after this node was appended,
    /// set after `repo.save_provenance_graph()` returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance_graph_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_cost_zero() {
        assert_eq!(compute_cost(0, 0), 0.0);
    }

    #[test]
    fn test_compute_cost_known_values() {
        let cost = compute_cost(16_083, 3_746);
        let expected = (16_083.0 / 1_000_000.0) * 5.0 + (3_746.0 / 1_000_000.0) * 25.0;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn test_phase_tokens_from_counts() {
        let pt = PhaseTokens::from_counts(1_000_000, 0);
        assert_eq!(pt.input, 1_000_000);
        assert_eq!(pt.output, 0);
        assert!((pt.cost_usd - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_phase_tokens_add() {
        let mut a = PhaseTokens::from_counts(500, 100);
        let b = PhaseTokens::from_counts(500, 100);
        a.add(&b);
        assert_eq!(a.input, 1000);
        assert_eq!(a.output, 200);
        assert!((a.cost_usd - compute_cost(1000, 200)).abs() < 1e-9);
    }

    #[test]
    fn test_turn_totals_from_phases() {
        let mut phases = HashMap::new();
        phases.insert("orienting".to_string(), PhaseTokens::from_counts(1000, 200));
        phases.insert("proposing".to_string(), PhaseTokens::from_counts(2000, 400));
        let totals = TurnTotals::from_phases(&phases);
        assert_eq!(totals.input, 3000);
        assert_eq!(totals.output, 600);
        assert_eq!(totals.total, 3600);
        assert!((totals.cost_usd - compute_cost(3000, 600)).abs() < 1e-9);
    }

    #[test]
    fn test_goal_detail_roundtrip() {
        let mut phases = HashMap::new();
        phases.insert("orienting".to_string(), PhaseTokens::from_counts(1842, 312));
        phases.insert("proposing".to_string(), PhaseTokens::from_counts(3201, 891));
        let turn_totals = TurnTotals::from_phases(&phases);
        let detail = GoalDetail {
            intent_title: "Build TypeScript Hello World CLI".into(),
            intent_description: "A spec.".into(),
            intent_turn_id: 3,
            issue_ref: Some("GH-42".into()),
            session_id: "tiny-star-8412".into(),
            phases,
            turn_totals,
            model: "anthropic/claude-sonnet-4-6".into(),
            context_pct: 9.9,
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: GoalDetail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.intent_title, detail.intent_title);
        assert_eq!(back.intent_turn_id, 3);
        assert_eq!(back.issue_ref.as_deref(), Some("GH-42"));
        assert!(back.phases.contains_key("orienting"));
    }

    #[test]
    fn test_commitment_detail_roundtrip() {
        let detail = CommitmentDetail {
            todo_id: "t2".into(),
            todo_content: "Install typescript as a dev dependency".into(),
            priority: "high".into(),
            contributor: Contributor::Ai,
            file: "package.json".into(),
            start_line: Some(1),
            end_line: Some(9),
            atomic_change_hash: None,
            tokens: PhaseTokens::from_counts(1203, 287),
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: CommitmentDetail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.todo_id, "t2");
        assert_eq!(back.contributor, Contributor::Ai);
        assert_eq!(back.start_line, Some(1));
        assert!(!json.contains("atomic_change_hash"));
    }

    #[test]
    fn test_execution_detail_roundtrip() {
        let detail = ExecutionDetail {
            todo_id: "t2".into(),
            command: "npm install --save-dev typescript".into(),
            exit_code: 0,
            duration_ms: 616,
            output_summary: "added 1 package".into(),
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: ExecutionDetail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.exit_code, 0);
        assert_eq!(back.duration_ms, 616);
    }

    #[test]
    fn test_verification_detail_roundtrip() {
        let detail = VerificationDetail {
            intent_turn_id: 3,
            intent_title: "Build TypeScript Hello World CLI".into(),
            outcome: TurnOutcome::Completed,
            summary: "npm start printed Hello World!".into(),
            turn_cost_usd: 0.015,
            turn_tokens_total: 19_829,
            learnings: Learnings {
                repo: vec!["tsc && node dist/index.js is the correct sequence".into()],
                workflow: vec![],
                code: vec![CodeFinding {
                    file: "src/index.ts".into(),
                    line: 1,
                    finding: "console.log is sufficient for CLI Hello World output".into(),
                }],
            },
            provenance_graph_hash: Some("ABCDEF123456".into()),
        };

        let json = serde_json::to_string(&detail).expect("serialize");
        let back: VerificationDetail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.outcome, TurnOutcome::Completed);
        assert_eq!(back.learnings.repo.len(), 1);
        assert_eq!(back.learnings.code[0].file, "src/index.ts");
        assert_eq!(back.provenance_graph_hash.as_deref(), Some("ABCDEF123456"));
        assert!(!json.contains("\"workflow\""));
    }

    #[test]
    fn test_contributor_display() {
        assert_eq!(Contributor::Ai.to_string(), "ai");
        assert_eq!(Contributor::Human.to_string(), "human");
        assert_eq!(Contributor::Mixed.to_string(), "mixed");
    }

    #[test]
    fn test_turn_outcome_display() {
        assert_eq!(TurnOutcome::Completed.to_string(), "completed");
        assert_eq!(TurnOutcome::Partial.to_string(), "partial");
        assert_eq!(TurnOutcome::Failed.to_string(), "failed");
    }

    #[test]
    fn test_learnings_is_empty() {
        let empty = Learnings::default();
        assert!(empty.is_empty());

        let non_empty = Learnings {
            repo: vec!["something".into()],
            ..Default::default()
        };
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn test_contributor_default_is_ai() {
        assert_eq!(Contributor::default(), Contributor::Ai);
    }

    #[test]
    fn test_turn_outcome_default_is_completed() {
        assert_eq!(TurnOutcome::default(), TurnOutcome::Completed);
    }
}

//! Type definitions for the provenance graph.
//!
//! Contains the node, edge, and statistics types used by `ProvenanceGraph`.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::types::Hash;

// =============================================================================
// ProvenanceNode
// =============================================================================

/// A node in the content-addressed provenance graph.
///
/// Structurally mirrors the accumulator's `GraphNode` from `atomic-agent`,
/// but uses `atomic-core` conventions for serialization (postcard-compatible,
/// `Hash` types where appropriate).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceNode {
    /// Unique identifier within this graph.
    pub id: String,

    /// What kind of activity this represents.
    pub kind: ProvenanceNodeKind,

    /// When this activity occurred (Unix epoch milliseconds).
    pub timestamp: i64,

    /// One-line human-readable summary.
    pub summary: String,

    /// Kind-specific structured data (JSON string).
    ///
    /// For split storage: this field may be stripped from the hashed
    /// version and stored separately in the unhashed section.
    #[serde(default)]
    pub detail: Option<String>,

    /// Link to the Atomic change hash this node produced.
    #[serde(default)]
    pub change_hash: Option<Hash>,

    /// Tool name that produced this node.
    #[serde(default)]
    pub tool_name: Option<String>,

    /// Tool call ID for correlating pre/post pairs.
    #[serde(default)]
    pub tool_call_id: Option<String>,

    /// Duration of the tool call in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,

    /// Whether this node was consolidated by the classifier.
    #[serde(default)]
    pub classified: bool,

    /// Classifier confidence (0.0–1.0).
    #[serde(default)]
    pub confidence: Option<f32>,

    /// IDs of raw tool call nodes this decision consolidates.
    #[serde(default)]
    pub consolidated_from: Vec<String>,
}

impl fmt::Display for ProvenanceNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.id, self.kind, self.summary)
    }
}

// =============================================================================
// ProvenanceNodeKind
// =============================================================================

/// The kind of activity a provenance graph node represents.
///
/// Mirrors `atomic_agent::provenance::types::NodeKind` but is independently
/// defined in `atomic-core` so the storage layer doesn't depend on the agent
/// crate.
///
/// ## Sherpa Workflow Mapping
///
/// When `ProvenanceGraph::profile == "sherpa-trace/1.0.0"` the following
/// variants are used with Sherpa-specific JSON payloads in `node.detail`:
///
/// | Variant        | Sherpa concept                                      | Detail struct       |
/// |----------------|-----------------------------------------------------|---------------------|
/// | `Goal`         | IntentNode — turn anchor with phase token breakdown | `GoalDetail`        |
/// | `Commitment`   | Todo that produced file changes, file attribution   | `CommitmentDetail`  |
/// | `Execution`    | `bash` tool call linked to the active todo          | `ExecutionDetail`   |
/// | `Verification` | Verification state — outcome + learnings            | `VerificationDetail`|
/// | `HumanGate`    | `/accept` or `/guide [ids]` resolution              | *(summary only)*    |
///
/// The remaining variants (`Exploration`, `Decision`, `PatchProposal`,
/// `Error`) are used by the generic `atomic-agent` path and are not emitted
/// by the Sherpa structured workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceNodeKind {
    /// Human prompt / Sherpa intent — what was asked for.
    ///
    /// In Sherpa graphs (`profile == "sherpa-trace/1.0.0"`) carries a
    /// `GoalDetail` JSON payload in `node.detail` with intent fields and
    /// per-phase token breakdown.
    Goal,
    /// Read/search/grep — understanding the codebase.
    Exploration,
    /// Consolidated: agent chose strategy X over Y.
    Decision,
    /// Write/edit/patch — file changes on disk.
    ///
    /// In Sherpa graphs one node is emitted per todo that produced file
    /// changes. Carries a `CommitmentDetail` JSON payload with todo fields,
    /// file attribution, and token slice.
    Commitment,
    /// Test/lint/typecheck/build — validating work.
    ///
    /// In Sherpa graphs written when the state machine reaches
    /// `State::Verification`. Carries a `VerificationDetail` JSON payload
    /// with outcome, learnings, and turn-level cost rollup.
    Verification,
    /// Bash (non-test) or other side effects.
    ///
    /// In Sherpa graphs one node is emitted per `bash` tool call. Carries an
    /// `ExecutionDetail` JSON payload with command, exit code, duration, and
    /// the active todo ID at time of call.
    Execution,
    /// Permission asked — agent uncertainty surfaced to human.
    ///
    /// In Sherpa graphs written on `/accept` or `/guide [ids]` resolution.
    /// The node summary records which path was taken; contributor attribution
    /// is propagated to the associated `CommitmentDetail` nodes.
    HumanGate,
    /// Recorded change — the output artifact.
    PatchProposal,
    /// Tool failure or session error.
    Error,
    /// A single todo item in the checklist (Sherpa only).
    ///
    /// Emitted during Proposing when the LLM registers todos via the tool.
    /// Carries content, priority, dependencies in `detail`.
    Todo,
    /// A todo item's status changed (Sherpa only).
    ///
    /// Tracks the full lifecycle: pending → in_progress → completed.
    /// Carries from_status, to_status, todo_id in `detail`.
    TodoStatusChange,
    /// A Petri net phase transition (Sherpa only).
    ///
    /// Records every state machine movement: orienting → proposing, etc.
    /// Carries from_phase, to_phase, trigger in `detail`.
    PhaseTransition,
    /// An unexpected failure or change of approach worth remembering (Sherpa only).
    ///
    /// Emitted by the LLM during Executing when something goes wrong.
    /// The label describes what was learned.
    Lesson,
    /// The LLM's final response for a phase (Sherpa only).
    ///
    /// Captures what the model actually said (the reply text) and any
    /// nodes it emitted. Distinct from Verification which is the outcome.
    LlmResponse,
    /// HumanGate resolution — which command the user chose (Sherpa only).
    ///
    /// Records `/accept`, `/guide [ids]`, `/reject`, `/revise` with the
    /// contributor attribution map.
    HumanGateResolution,
}

impl ProvenanceNodeKind {
    /// Short label for display.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Exploration => "exploration",
            Self::Decision => "decision",
            Self::Commitment => "commitment",
            Self::Verification => "verification",
            Self::Execution => "execution",
            Self::HumanGate => "human_gate",
            Self::PatchProposal => "patch_proposal",
            Self::Error => "error",
            Self::Todo => "todo",
            Self::TodoStatusChange => "todo_status_change",
            Self::PhaseTransition => "phase_transition",
            Self::Lesson => "lesson",
            Self::LlmResponse => "llm_response",
            Self::HumanGateResolution => "human_gate_resolution",
        }
    }
}

impl fmt::Display for ProvenanceNodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// =============================================================================
// ProvenanceEdge
// =============================================================================

/// A causal edge between two provenance nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceEdge {
    /// Source node ID (the cause).
    pub from: String,
    /// Target node ID (the effect).
    pub to: String,
    /// The kind of causal relationship.
    pub kind: ProvenanceEdgeKind,
}

impl fmt::Display for ProvenanceEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} --{}--▶ {}", self.from, self.kind, self.to)
    }
}

// =============================================================================
// ProvenanceEdgeKind
// =============================================================================

/// The kind of causal relationship between two provenance nodes.
///
/// Mirrors `atomic_agent::provenance::types::EdgeKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceEdgeKind {
    /// A led to B (forward-causal).
    LedTo,
    /// Decision/commitment was informed by exploration.
    ExploredVia,
    /// Decision produced file commitment.
    CommittedVia,
    /// Commitment was validated by verification.
    VerifiedBy,
    /// Human gate blocked progress.
    BlockedBy,
    /// Work continued after human gate resolved.
    ResumedAfter,
    /// Tool call produced an error.
    FailedWith,
}

impl ProvenanceEdgeKind {
    /// Short label for display.
    pub fn label(&self) -> &'static str {
        match self {
            Self::LedTo => "led_to",
            Self::ExploredVia => "explored_via",
            Self::CommittedVia => "committed_via",
            Self::VerifiedBy => "verified_by",
            Self::BlockedBy => "blocked_by",
            Self::ResumedAfter => "resumed_after",
            Self::FailedWith => "failed_with",
        }
    }
}

impl fmt::Display for ProvenanceEdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// =============================================================================
// ProvenanceStats
// =============================================================================

/// Aggregate statistics for a provenance graph.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceStats {
    pub goal_count: u32,
    pub exploration_count: u32,
    pub decision_count: u32,
    pub commitment_count: u32,
    pub verification_count: u32,
    pub human_gate_count: u32,
    pub error_count: u32,
    pub execution_count: u32,
    pub patch_proposal_count: u32,
    pub edge_count: u32,
    #[serde(default)]
    pub todo_count: u32,
    #[serde(default)]
    pub todo_status_change_count: u32,
    #[serde(default)]
    pub phase_transition_count: u32,
    #[serde(default)]
    pub lesson_count: u32,
    #[serde(default)]
    pub llm_response_count: u32,
    #[serde(default)]
    pub human_gate_resolution_count: u32,
}

impl ProvenanceStats {
    /// Total number of nodes across all kinds.
    pub fn total_nodes(&self) -> u32 {
        self.goal_count
            + self.exploration_count
            + self.decision_count
            + self.commitment_count
            + self.verification_count
            + self.human_gate_count
            + self.error_count
            + self.execution_count
            + self.patch_proposal_count
            + self.todo_count
            + self.todo_status_change_count
            + self.phase_transition_count
            + self.lesson_count
            + self.llm_response_count
            + self.human_gate_resolution_count
    }

    /// Increment the counter for the given node kind.
    pub fn increment(&mut self, kind: ProvenanceNodeKind) {
        match kind {
            ProvenanceNodeKind::Goal => self.goal_count += 1,
            ProvenanceNodeKind::Exploration => self.exploration_count += 1,
            ProvenanceNodeKind::Decision => self.decision_count += 1,
            ProvenanceNodeKind::Commitment => self.commitment_count += 1,
            ProvenanceNodeKind::Verification => self.verification_count += 1,
            ProvenanceNodeKind::Execution => self.execution_count += 1,
            ProvenanceNodeKind::HumanGate => self.human_gate_count += 1,
            ProvenanceNodeKind::PatchProposal => self.patch_proposal_count += 1,
            ProvenanceNodeKind::Error => self.error_count += 1,
            ProvenanceNodeKind::Todo => self.todo_count += 1,
            ProvenanceNodeKind::TodoStatusChange => self.todo_status_change_count += 1,
            ProvenanceNodeKind::PhaseTransition => self.phase_transition_count += 1,
            ProvenanceNodeKind::Lesson => self.lesson_count += 1,
            ProvenanceNodeKind::LlmResponse => self.llm_response_count += 1,
            ProvenanceNodeKind::HumanGateResolution => self.human_gate_resolution_count += 1,
        }
    }

    /// Returns `true` if there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.total_nodes() == 0
    }

    /// Compute stats from a slice of nodes and edges.
    pub fn from_graph(nodes: &[ProvenanceNode], edges: &[ProvenanceEdge]) -> Self {
        let mut stats = Self::default();
        for node in nodes {
            stats.increment(node.kind);
        }
        stats.edge_count = edges.len() as u32;
        stats
    }
}

impl fmt::Display for ProvenanceStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();

        macro_rules! push_stat {
            ($count:expr, $singular:expr, $plural:expr) => {
                if $count > 0 {
                    if $count == 1 {
                        parts.push(format!("1 {}", $singular));
                    } else {
                        parts.push(format!("{} {}", $count, $plural));
                    }
                }
            };
        }

        push_stat!(self.goal_count, "goal", "goals");
        push_stat!(self.exploration_count, "exploration", "explorations");
        push_stat!(self.decision_count, "decision", "decisions");
        push_stat!(self.commitment_count, "commitment", "commitments");
        push_stat!(self.verification_count, "verification", "verifications");
        push_stat!(self.execution_count, "execution", "executions");
        push_stat!(self.human_gate_count, "gate", "gates");
        push_stat!(self.patch_proposal_count, "patch", "patches");
        push_stat!(self.error_count, "error", "errors");

        if parts.is_empty() {
            write!(f, "empty graph")
        } else {
            write!(
                f,
                "{} ({} edge{})",
                parts.join(", "),
                self.edge_count,
                if self.edge_count == 1 { "" } else { "s" }
            )
        }
    }
}

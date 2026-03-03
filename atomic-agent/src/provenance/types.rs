//! Provenance graph type definitions.
//!
//! These types represent the nodes, edges, and metadata of a provenance DAG
//! that captures the causal decision chain of an AI agent session. The graph
//! is built incrementally by the [`super::accumulator::ProvenanceAccumulator`]
//! as agent hook events arrive, and serialized for persistence and transport
//! by [`super::serialize`].
//!
//! # Node Kinds
//!
//! Each node represents a distinct type of agent activity:
//!
//! | Kind | Source event | Example |
//! |------|-------------|---------|
//! | `Goal` | `TurnStart` (user prompt) | "Fix the auth bug in login.rs" |
//! | `Exploration` | `PostToolUse` (read/grep) | "Read src/auth/login.rs" |
//! | `Decision` | Classification (Phase 3) | "Explored auth → chose JWT fix" |
//! | `Commitment` | `PostToolUse` (edit/write) | "Edit src/auth/login.rs" |
//! | `Verification` | `PostToolUse` (test/lint) | "cargo test --lib (passed)" |
//! | `Execution` | `PostToolUse` (bash/other) | "npm install jsonwebtoken" |
//! | `HumanGate` | Permission hook | "Awaiting approval: delete old tokens?" |
//! | `PatchProposal` | `TurnEnd` (change recorded) | "Change ABCD: +12 -3 lines" |
//! | `Error` | `PostToolUse` (status=error) | "Edit failed: file not found" |
//!
//! # Edge Kinds
//!
//! Edges represent causal relationships between nodes:
//!
//! ```text
//! Goal ──led_to──▶ Exploration ──explored_via──▶ Commitment ──verified_by──▶ Verification
//!                                                    │
//!                                                    └──committed_via──▶ PatchProposal
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

// =============================================================================
// NodeKind
// =============================================================================

/// The kind of activity a provenance graph node represents.
///
/// Each variant maps to a distinct class of agent behavior. The classifier
/// in [`super::classify`] determines the kind from tool names and arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Human prompt — what was asked for.
    Goal,

    /// Read/search/grep — understanding the codebase.
    Exploration,

    /// Consolidated decision: agent chose strategy X over Y.
    ///
    /// Created by the Phase 3 classification layer, not by raw events.
    /// A decision node references the raw tool call nodes it consolidates
    /// via [`GraphNode::consolidated_from`].
    Decision,

    /// Write/edit/patch — file changes on disk.
    Commitment,

    /// Test/lint/typecheck/build — validating work.
    Verification,

    /// Bash (non-test, non-lint, non-build) or other side effects.
    Execution,

    /// Permission asked — agent uncertainty surfaced to human.
    HumanGate,

    /// Recorded change — the output artifact of a turn.
    PatchProposal,

    /// Tool failure or session error.
    Error,
}

impl NodeKind {
    /// Returns `true` if this kind represents a tool-derived node.
    pub fn is_tool_derived(&self) -> bool {
        matches!(
            self,
            NodeKind::Exploration
                | NodeKind::Commitment
                | NodeKind::Verification
                | NodeKind::Execution
                | NodeKind::Error
        )
    }

    /// Returns `true` if this kind represents a file-modifying activity.
    pub fn is_mutating(&self) -> bool {
        matches!(self, NodeKind::Commitment | NodeKind::Execution)
    }

    /// Short label for display and logging.
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Goal => "goal",
            NodeKind::Exploration => "exploration",
            NodeKind::Decision => "decision",
            NodeKind::Commitment => "commitment",
            NodeKind::Verification => "verification",
            NodeKind::Execution => "execution",
            NodeKind::HumanGate => "human_gate",
            NodeKind::PatchProposal => "patch_proposal",
            NodeKind::Error => "error",
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// =============================================================================
// EdgeKind
// =============================================================================

/// The kind of causal relationship between two provenance nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A led to B (forward-causal). The general-purpose causal edge.
    LedTo,

    /// A decision or commitment was informed by exploration A.
    ExploredVia,

    /// A decision produced file commitment B.
    CommittedVia,

    /// Commitment A was validated by verification B.
    VerifiedBy,

    /// Human gate A blocked progress on B.
    BlockedBy,

    /// Work B continued after human gate A was resolved.
    ResumedAfter,

    /// Tool call A produced error B.
    FailedWith,
}

impl EdgeKind {
    /// Short label for display and logging.
    pub fn label(&self) -> &'static str {
        match self {
            EdgeKind::LedTo => "led_to",
            EdgeKind::ExploredVia => "explored_via",
            EdgeKind::CommittedVia => "committed_via",
            EdgeKind::VerifiedBy => "verified_by",
            EdgeKind::BlockedBy => "blocked_by",
            EdgeKind::ResumedAfter => "resumed_after",
            EdgeKind::FailedWith => "failed_with",
        }
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// =============================================================================
// GraphNode
// =============================================================================

/// A node in the provenance graph.
///
/// Each node represents a single activity in the agent's decision chain.
/// Nodes are created by the accumulator as events arrive and are immutable
/// once created (the graph is append-only).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphNode {
    /// Unique identifier within this session graph.
    ///
    /// Format: `{session_id_short}-{counter}` where `session_id_short` is
    /// the first 8 characters of the session ID.
    pub id: String,

    /// What kind of activity this represents.
    pub kind: NodeKind,

    /// When this activity occurred (Unix epoch milliseconds).
    pub timestamp: i64,

    /// One-line human-readable summary.
    ///
    /// Examples:
    /// - `"Fix the auth bug in login.rs"` (goal)
    /// - `"Read src/auth/login.rs"` (exploration)
    /// - `"Edit src/auth/login.rs"` (commitment)
    /// - `"cargo test --lib (passed)"` (verification)
    pub summary: String,

    /// Kind-specific structured data.
    ///
    /// The schema depends on the node kind:
    /// - Exploration: `{ "files_read": ["src/auth.rs"] }`
    /// - Commitment: `{ "files_modified": ["src/auth.rs"], "tool": "edit" }`
    /// - Verification: `{ "command": "cargo test", "passed": true }`
    /// - HumanGate: `{ "reason": "...", "resolved": false }`
    /// - PatchProposal: `{ "files": ["src/auth.rs"], "change_hash": "ABCD..." }`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,

    /// Link to the Atomic change hash this node produced.
    ///
    /// Only set for `Commitment` and `PatchProposal` nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_hash: Option<String>,

    /// Tool name that produced this node (e.g., "read", "edit", "bash").
    ///
    /// Only set for tool-derived nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,

    /// Tool call ID for correlating pre/post pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Duration of the tool call in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    /// Whether this node was consolidated by the classifier (Phase 3).
    ///
    /// Raw tool call nodes have `classified = false`. Decision nodes
    /// created by the classification layer have `classified = true`.
    #[serde(default)]
    pub classified: bool,

    /// Classifier confidence score (0.0–1.0).
    ///
    /// Only meaningful when `classified = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,

    /// IDs of raw tool call nodes this decision consolidates.
    ///
    /// Only populated for `Decision` nodes created by the Phase 3
    /// classification layer. Empty for all other node kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consolidated_from: Vec<String>,
}

impl GraphNode {
    /// Create a new node with the minimum required fields.
    pub fn new(
        id: impl Into<String>,
        kind: NodeKind,
        timestamp: i64,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            timestamp,
            summary: summary.into(),
            detail: None,
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        }
    }

    /// Set the detail field.
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Set the change hash.
    pub fn with_change_hash(mut self, hash: impl Into<String>) -> Self {
        self.change_hash = Some(hash.into());
        self
    }

    /// Set the tool name.
    pub fn with_tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = Some(name.into());
        self
    }

    /// Set the tool call ID.
    pub fn with_tool_call_id(mut self, id: impl Into<String>) -> Self {
        self.tool_call_id = Some(id.into());
        self
    }

    /// Set the duration.
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
}

impl fmt::Display for GraphNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.id, self.kind, self.summary)
    }
}

// =============================================================================
// GraphEdge
// =============================================================================

/// A causal edge between two provenance nodes.
///
/// Edges are directed: `from` is the cause/antecedent, `to` is the
/// effect/consequent. The accumulator infers edges automatically based
/// on the sequence of events and their classifications.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source node ID (the cause).
    pub from: String,

    /// Target node ID (the effect).
    pub to: String,

    /// The kind of causal relationship.
    pub kind: EdgeKind,
}

impl GraphEdge {
    /// Create a new edge.
    pub fn new(from: impl Into<String>, to: impl Into<String>, kind: EdgeKind) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
        }
    }
}

impl fmt::Display for GraphEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} --{}--▶ {}", self.from, self.kind, self.to)
    }
}

// =============================================================================
// GraphStats
// =============================================================================

/// Aggregate statistics for a provenance graph.
///
/// Computed from the graph's nodes and edges. Used for logging, display,
/// and quick filtering without loading the full graph.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
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
}

impl GraphStats {
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
    }

    /// Increment the counter for the given node kind.
    pub fn increment(&mut self, kind: NodeKind) {
        match kind {
            NodeKind::Goal => self.goal_count += 1,
            NodeKind::Exploration => self.exploration_count += 1,
            NodeKind::Decision => self.decision_count += 1,
            NodeKind::Commitment => self.commitment_count += 1,
            NodeKind::Verification => self.verification_count += 1,
            NodeKind::Execution => self.execution_count += 1,
            NodeKind::HumanGate => self.human_gate_count += 1,
            NodeKind::PatchProposal => self.patch_proposal_count += 1,
            NodeKind::Error => self.error_count += 1,
        }
    }

    /// Returns `true` if there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.total_nodes() == 0
    }
}

impl fmt::Display for GraphStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();

        if self.goal_count > 0 {
            parts.push(format!(
                "{} goal{}",
                self.goal_count,
                plural(self.goal_count)
            ));
        }
        if self.exploration_count > 0 {
            parts.push(format!(
                "{} exploration{}",
                self.exploration_count,
                plural(self.exploration_count)
            ));
        }
        if self.decision_count > 0 {
            parts.push(format!(
                "{} decision{}",
                self.decision_count,
                plural(self.decision_count)
            ));
        }
        if self.commitment_count > 0 {
            parts.push(format!(
                "{} commitment{}",
                self.commitment_count,
                plural(self.commitment_count)
            ));
        }
        if self.verification_count > 0 {
            parts.push(format!(
                "{} verification{}",
                self.verification_count,
                plural(self.verification_count)
            ));
        }
        if self.execution_count > 0 {
            parts.push(format!(
                "{} execution{}",
                self.execution_count,
                plural(self.execution_count)
            ));
        }
        if self.human_gate_count > 0 {
            parts.push(format!(
                "{} gate{}",
                self.human_gate_count,
                plural(self.human_gate_count)
            ));
        }
        if self.patch_proposal_count > 0 {
            parts.push(format!(
                "{} patch{}",
                self.patch_proposal_count,
                plural_es(self.patch_proposal_count)
            ));
        }
        if self.error_count > 0 {
            parts.push(format!(
                "{} error{}",
                self.error_count,
                plural(self.error_count)
            ));
        }

        if parts.is_empty() {
            write!(f, "empty graph")
        } else {
            write!(
                f,
                "{} ({} edge{})",
                parts.join(", "),
                self.edge_count,
                plural(self.edge_count)
            )
        }
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn plural_es(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "es"
    }
}

// =============================================================================
// SerializedGraph
// =============================================================================

/// The complete serialized form of a provenance graph.
///
/// This is what gets written to `.atomic/sessions/{id}/graph.json` and
/// what the compaction hook reads from disk. It contains the full graph
/// state including the accumulator's internal counters needed to resume
/// appending after a process restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializedGraph {
    /// Schema version. Always 1 for now.
    pub version: u32,

    /// Session this graph belongs to.
    pub session_id: String,

    /// When this serialization was created (Unix epoch ms).
    pub created_at: i64,

    /// All nodes, ordered by timestamp.
    pub nodes: Vec<GraphNode>,

    /// All causal edges.
    pub edges: Vec<GraphEdge>,

    /// Aggregate statistics.
    pub stats: GraphStats,

    /// Monotonic counter for generating the next node ID.
    ///
    /// Persisted so the accumulator can resume after process restart
    /// without ID collisions.
    pub counter: u64,

    // ---- Accumulator state for resumption ----
    /// Most recent goal node ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_goal: Option<String>,

    /// Exploration node IDs accumulated since the last commitment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_explorations: Vec<String>,

    /// Most recent commitment node ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commitment: Option<String>,

    /// Most recent node ID (any kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_node: Option<String>,

    /// Human gate node ID that is currently blocking, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_human_gate: Option<String>,

    /// Base32 hash of the last content-addressed ProvenanceGraph artifact
    /// saved for this session. Used to chain per-turn provenance graphs
    /// via the `previous` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_provenance_hash: Option<String>,

    /// Number of nodes included in the last saved ProvenanceGraph.
    /// Used by the accumulator to export only the per-turn delta
    /// (nodes added since the last save) instead of the full cumulative set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes_saved_count: Option<usize>,

    /// Number of edges included in the last saved ProvenanceGraph.
    /// Used alongside `nodes_saved_count` for per-turn delta export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges_saved_count: Option<usize>,
}

impl SerializedGraph {
    /// Current schema version.
    pub const VERSION: u32 = 1;
}

impl Default for SerializedGraph {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            session_id: String::new(),
            created_at: 0,
            nodes: Vec::new(),
            edges: Vec::new(),
            stats: GraphStats::default(),
            counter: 0,
            current_goal: None,
            pending_explorations: Vec::new(),
            last_commitment: None,
            last_node: None,
            pending_human_gate: None,
            last_provenance_hash: None,
            nodes_saved_count: None,
            edges_saved_count: None,
        }
    }
}

impl fmt::Display for SerializedGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProvenanceGraph(session={}, {} nodes, {} edges)",
            self.session_id,
            self.nodes.len(),
            self.edges.len(),
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- NodeKind ----

    #[test]
    fn test_node_kind_label_roundtrip() {
        let kinds = [
            NodeKind::Goal,
            NodeKind::Exploration,
            NodeKind::Decision,
            NodeKind::Commitment,
            NodeKind::Verification,
            NodeKind::Execution,
            NodeKind::HumanGate,
            NodeKind::PatchProposal,
            NodeKind::Error,
        ];
        for kind in &kinds {
            let label = kind.label();
            assert!(
                !label.is_empty(),
                "label for {:?} should not be empty",
                kind
            );
            assert_eq!(kind.to_string(), label);
        }
    }

    #[test]
    fn test_node_kind_is_tool_derived() {
        assert!(!NodeKind::Goal.is_tool_derived());
        assert!(NodeKind::Exploration.is_tool_derived());
        assert!(!NodeKind::Decision.is_tool_derived());
        assert!(NodeKind::Commitment.is_tool_derived());
        assert!(NodeKind::Verification.is_tool_derived());
        assert!(NodeKind::Execution.is_tool_derived());
        assert!(!NodeKind::HumanGate.is_tool_derived());
        assert!(!NodeKind::PatchProposal.is_tool_derived());
        assert!(NodeKind::Error.is_tool_derived());
    }

    #[test]
    fn test_node_kind_is_mutating() {
        assert!(NodeKind::Commitment.is_mutating());
        assert!(NodeKind::Execution.is_mutating());
        assert!(!NodeKind::Exploration.is_mutating());
        assert!(!NodeKind::Verification.is_mutating());
        assert!(!NodeKind::Goal.is_mutating());
    }

    #[test]
    fn test_node_kind_serde_roundtrip() {
        let kinds = [
            NodeKind::Goal,
            NodeKind::Exploration,
            NodeKind::Decision,
            NodeKind::Commitment,
            NodeKind::Verification,
            NodeKind::Execution,
            NodeKind::HumanGate,
            NodeKind::PatchProposal,
            NodeKind::Error,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: NodeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    #[test]
    fn test_node_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&NodeKind::HumanGate).unwrap(),
            "\"human_gate\""
        );
        assert_eq!(
            serde_json::to_string(&NodeKind::PatchProposal).unwrap(),
            "\"patch_proposal\""
        );
        assert_eq!(serde_json::to_string(&NodeKind::Goal).unwrap(), "\"goal\"");
    }

    // ---- EdgeKind ----

    #[test]
    fn test_edge_kind_label_roundtrip() {
        let kinds = [
            EdgeKind::LedTo,
            EdgeKind::ExploredVia,
            EdgeKind::CommittedVia,
            EdgeKind::VerifiedBy,
            EdgeKind::BlockedBy,
            EdgeKind::ResumedAfter,
            EdgeKind::FailedWith,
        ];
        for kind in &kinds {
            let label = kind.label();
            assert!(!label.is_empty());
            assert_eq!(kind.to_string(), label);
        }
    }

    #[test]
    fn test_edge_kind_serde_roundtrip() {
        let kinds = [
            EdgeKind::LedTo,
            EdgeKind::ExploredVia,
            EdgeKind::CommittedVia,
            EdgeKind::VerifiedBy,
            EdgeKind::BlockedBy,
            EdgeKind::ResumedAfter,
            EdgeKind::FailedWith,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: EdgeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    #[test]
    fn test_edge_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&EdgeKind::LedTo).unwrap(),
            "\"led_to\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeKind::ExploredVia).unwrap(),
            "\"explored_via\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeKind::CommittedVia).unwrap(),
            "\"committed_via\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeKind::VerifiedBy).unwrap(),
            "\"verified_by\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeKind::FailedWith).unwrap(),
            "\"failed_with\""
        );
    }

    // ---- GraphNode ----

    #[test]
    fn test_graph_node_new() {
        let node = GraphNode::new("test-1", NodeKind::Goal, 1000, "Fix the bug");
        assert_eq!(node.id, "test-1");
        assert_eq!(node.kind, NodeKind::Goal);
        assert_eq!(node.timestamp, 1000);
        assert_eq!(node.summary, "Fix the bug");
        assert!(node.detail.is_none());
        assert!(node.change_hash.is_none());
        assert!(node.tool_name.is_none());
        assert!(node.tool_call_id.is_none());
        assert!(node.duration_ms.is_none());
        assert!(!node.classified);
        assert!(node.confidence.is_none());
        assert!(node.consolidated_from.is_empty());
    }

    #[test]
    fn test_graph_node_builder() {
        let node = GraphNode::new("test-2", NodeKind::Commitment, 2000, "Edit main.rs")
            .with_tool_name("edit")
            .with_tool_call_id("call-42")
            .with_duration_ms(150)
            .with_change_hash("ABCD1234")
            .with_detail(serde_json::json!({"files_modified": ["src/main.rs"]}));

        assert_eq!(node.tool_name.as_deref(), Some("edit"));
        assert_eq!(node.tool_call_id.as_deref(), Some("call-42"));
        assert_eq!(node.duration_ms, Some(150));
        assert_eq!(node.change_hash.as_deref(), Some("ABCD1234"));
        assert!(node.detail.is_some());
    }

    #[test]
    fn test_graph_node_display() {
        let node = GraphNode::new("s-1", NodeKind::Exploration, 1000, "Read auth.rs");
        let display = format!("{}", node);
        assert!(display.contains("s-1"));
        assert!(display.contains("exploration"));
        assert!(display.contains("Read auth.rs"));
    }

    #[test]
    fn test_graph_node_serde_roundtrip() {
        let node = GraphNode::new("s-1", NodeKind::Commitment, 1234, "Edit file")
            .with_tool_name("edit")
            .with_detail(serde_json::json!({"key": "value"}));

        let json = serde_json::to_string(&node).unwrap();
        let back: GraphNode = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, "s-1");
        assert_eq!(back.kind, NodeKind::Commitment);
        assert_eq!(back.timestamp, 1234);
        assert_eq!(back.summary, "Edit file");
        assert_eq!(back.tool_name.as_deref(), Some("edit"));
        assert!(back.detail.is_some());
    }

    #[test]
    fn test_graph_node_serde_skips_none() {
        let node = GraphNode::new("s-1", NodeKind::Goal, 1000, "Test");
        let json = serde_json::to_string(&node).unwrap();

        // None fields with skip_serializing_if should not appear in JSON
        assert!(!json.contains("detail"));
        assert!(!json.contains("change_hash"));
        assert!(!json.contains("tool_name"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("duration_ms"));
        assert!(!json.contains("confidence"));
        assert!(!json.contains("consolidated_from"));
    }

    // ---- GraphEdge ----

    #[test]
    fn test_graph_edge_new() {
        let edge = GraphEdge::new("a", "b", EdgeKind::LedTo);
        assert_eq!(edge.from, "a");
        assert_eq!(edge.to, "b");
        assert_eq!(edge.kind, EdgeKind::LedTo);
    }

    #[test]
    fn test_graph_edge_display() {
        let edge = GraphEdge::new("node-1", "node-2", EdgeKind::ExploredVia);
        let display = format!("{}", edge);
        assert!(display.contains("node-1"));
        assert!(display.contains("node-2"));
        assert!(display.contains("explored_via"));
    }

    #[test]
    fn test_graph_edge_serde_roundtrip() {
        let edge = GraphEdge::new("a", "b", EdgeKind::VerifiedBy);
        let json = serde_json::to_string(&edge).unwrap();
        let back: GraphEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, back);
    }

    // ---- GraphStats ----

    #[test]
    fn test_graph_stats_default_is_empty() {
        let stats = GraphStats::default();
        assert!(stats.is_empty());
        assert_eq!(stats.total_nodes(), 0);
    }

    #[test]
    fn test_graph_stats_increment() {
        let mut stats = GraphStats::default();
        stats.increment(NodeKind::Goal);
        stats.increment(NodeKind::Exploration);
        stats.increment(NodeKind::Exploration);
        stats.increment(NodeKind::Commitment);
        stats.edge_count = 3;

        assert_eq!(stats.goal_count, 1);
        assert_eq!(stats.exploration_count, 2);
        assert_eq!(stats.commitment_count, 1);
        assert_eq!(stats.total_nodes(), 4);
        assert!(!stats.is_empty());
    }

    #[test]
    fn test_graph_stats_increment_all_kinds() {
        let mut stats = GraphStats::default();
        let kinds = [
            NodeKind::Goal,
            NodeKind::Exploration,
            NodeKind::Decision,
            NodeKind::Commitment,
            NodeKind::Verification,
            NodeKind::Execution,
            NodeKind::HumanGate,
            NodeKind::PatchProposal,
            NodeKind::Error,
        ];
        for kind in &kinds {
            stats.increment(*kind);
        }
        assert_eq!(stats.total_nodes(), 9);
    }

    #[test]
    fn test_graph_stats_display_empty() {
        let stats = GraphStats::default();
        assert_eq!(format!("{}", stats), "empty graph");
    }

    #[test]
    fn test_graph_stats_display_with_nodes() {
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.exploration_count = 3;
        stats.commitment_count = 1;
        stats.edge_count = 4;
        let display = format!("{}", stats);

        assert!(display.contains("1 goal"));
        assert!(display.contains("3 explorations"));
        assert!(display.contains("1 commitment"));
        assert!(display.contains("4 edges"));
    }

    #[test]
    fn test_graph_stats_display_singular_vs_plural() {
        let mut stats = GraphStats::default();
        stats.goal_count = 1;
        stats.edge_count = 1;
        let display = format!("{}", stats);
        assert!(display.contains("1 goal,") || display.contains("1 goal ("));
        assert!(display.contains("1 edge)"));

        stats.goal_count = 2;
        stats.edge_count = 2;
        let display = format!("{}", stats);
        assert!(display.contains("2 goals"));
        assert!(display.contains("2 edges"));
    }

    #[test]
    fn test_graph_stats_serde_roundtrip() {
        let mut stats = GraphStats::default();
        stats.goal_count = 2;
        stats.exploration_count = 5;
        stats.commitment_count = 3;
        stats.verification_count = 1;
        stats.edge_count = 10;

        let json = serde_json::to_string(&stats).unwrap();
        let back: GraphStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    // ---- SerializedGraph ----

    #[test]
    fn test_serialized_graph_default() {
        let graph = SerializedGraph::default();
        assert_eq!(graph.version, SerializedGraph::VERSION);
        assert!(graph.session_id.is_empty());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.stats.is_empty());
        assert_eq!(graph.counter, 0);
        assert!(graph.current_goal.is_none());
        assert!(graph.pending_explorations.is_empty());
        assert!(graph.last_commitment.is_none());
        assert!(graph.last_node.is_none());
        assert!(graph.pending_human_gate.is_none());
        assert!(graph.last_provenance_hash.is_none());
    }

    #[test]
    fn test_serialized_graph_display() {
        let mut graph = SerializedGraph::default();
        graph.session_id = "test-123".into();
        graph
            .nodes
            .push(GraphNode::new("n1", NodeKind::Goal, 1000, "Test"));
        graph
            .edges
            .push(GraphEdge::new("n1", "n2", EdgeKind::LedTo));

        let display = format!("{}", graph);
        assert!(display.contains("test-123"));
        assert!(display.contains("1 nodes"));
        assert!(display.contains("1 edges"));
    }

    #[test]
    fn test_serialized_graph_serde_roundtrip() {
        let mut graph = SerializedGraph::default();
        graph.session_id = "sess-abc".into();
        graph.created_at = 1735689600000;
        graph.counter = 5;
        graph.current_goal = Some("n-1".into());
        graph.pending_explorations = vec!["n-2".into(), "n-3".into()];
        graph.last_commitment = Some("n-4".into());
        graph.last_node = Some("n-4".into());

        graph
            .nodes
            .push(GraphNode::new("n-1", NodeKind::Goal, 1000, "Fix bug"));
        graph.nodes.push(
            GraphNode::new("n-2", NodeKind::Exploration, 1001, "Read file").with_tool_name("read"),
        );
        graph
            .edges
            .push(GraphEdge::new("n-1", "n-2", EdgeKind::LedTo));

        graph.stats.goal_count = 1;
        graph.stats.exploration_count = 1;
        graph.stats.edge_count = 1;

        let json = serde_json::to_string_pretty(&graph).unwrap();
        let back: SerializedGraph = serde_json::from_str(&json).unwrap();

        assert_eq!(back.version, SerializedGraph::VERSION);
        assert_eq!(back.session_id, "sess-abc");
        assert_eq!(back.counter, 5);
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.edges.len(), 1);
        assert_eq!(back.current_goal.as_deref(), Some("n-1"));
        assert_eq!(back.pending_explorations, vec!["n-2", "n-3"]);
        assert_eq!(back.last_commitment.as_deref(), Some("n-4"));
        assert_eq!(back.stats.goal_count, 1);
    }

    #[test]
    fn test_serialized_graph_skips_empty_accumulator_state() {
        let graph = SerializedGraph::default();
        let json = serde_json::to_string(&graph).unwrap();

        // Accumulator state fields with skip_serializing_if should not appear
        assert!(!json.contains("current_goal"));
        assert!(!json.contains("pending_explorations"));
        assert!(!json.contains("last_commitment"));
        assert!(!json.contains("last_node"));
        assert!(!json.contains("pending_human_gate"));
        assert!(!json.contains("last_provenance_hash"));
    }

    #[test]
    fn test_serialized_graph_deserialize_without_accumulator_state() {
        // Older versions of the graph file won't have accumulator state fields.
        // Ensure they deserialize with defaults.
        let json = r#"{
            "version": 1,
            "session_id": "test",
            "created_at": 0,
            "nodes": [],
            "edges": [],
            "stats": {
                "goal_count": 0,
                "exploration_count": 0,
                "decision_count": 0,
                "commitment_count": 0,
                "verification_count": 0,
                "human_gate_count": 0,
                "error_count": 0,
                "execution_count": 0,
                "patch_proposal_count": 0,
                "edge_count": 0
            },
            "counter": 0
        }"#;

        let graph: SerializedGraph = serde_json::from_str(json).unwrap();
        assert!(graph.current_goal.is_none());
        assert!(graph.pending_explorations.is_empty());
        assert!(graph.last_commitment.is_none());
        assert!(graph.last_node.is_none());
        assert!(graph.pending_human_gate.is_none());
    }
}

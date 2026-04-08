//! In-memory DAG builder for session provenance graphs.
//!
//! ## Conversion to Content-Addressed Storage
//!
//! The accumulator maintains the graph using `atomic-agent` types (JSON-serialized,
//! `String` change hashes). When a turn is recorded, [`ProvenanceAccumulator::to_provenance_graph`]
//! converts the accumulated state to the `atomic-core` [`atomic_core::change::provenance_graph::ProvenanceGraph`] type
//! (postcard-serialized, `Hash` change hashes) for content-addressed storage.
//!
//! The [`ProvenanceAccumulator`] maintains the provenance graph for a single
//! agent session. It is loaded from disk at the start of each hook invocation,
//! appended to as events arrive, and saved back to disk before the process
//! exits. This design works with the "each hook is a separate process" model
//! used by `atomic agent hooks`.
//!
//! # Persistence
//!
//! The graph is stored as JSON at `.atomic/sessions/{session_id}/graph.json`.
//! Reads and writes are atomic (write to temp file, then rename) to prevent
//! corruption from process crashes.
//!
//! # Edge Inference
//!
//! Edges are inferred automatically from the sequence of events and their
//! classifications. The accumulator tracks "cursor" state — the current goal,
//! pending explorations, last commitment, etc. — to determine which edges
//! to create when a new node is appended.
//!
//! | New node kind   | Edge(s) created                                              |
//! |-----------------|--------------------------------------------------------------|
//! | Goal            | prev_goal --led_to-→ new_goal (if chained)                   |
//! | Exploration     | current_goal --led_to-→ exploration                          |
//! | Commitment      | each pending exploration --explored_via-→ commitment;        |
//! |                 | current_goal --led_to-→ commitment (if no explorations)      |
//! | Verification    | last_commitment --verified_by-→ verification                 |
//! | Execution       | current_goal --led_to-→ execution                            |
//! | Error           | last_node --failed_with-→ error                              |
//! | HumanGate       | last_node --blocked_by-→ gate                                |
//! | PatchProposal   | each commitment since last patch --committed_via-→ patch     |
//!
//! # Example
//!
//! ```rust,no_run
//! use atomic_agent::provenance::accumulator::ProvenanceAccumulator;
//!
//! let mut acc = ProvenanceAccumulator::new("test-session");
//!
//! // Human asks to fix a bug
//! let goal = acc.append_goal("Fix the auth bug", 1000);
//!
//! // Agent reads files
//! let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
//! let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
//!
//! // Agent edits a file
//! let edit = acc.append_tool_call("edit", Some("c3"), None, None, None, None, 1003);
//!
//! // Agent runs tests
//! let test_input = serde_json::json!({"command": "cargo test"});
//! let test = acc.append_tool_call("bash", Some("c4"), Some(&test_input), None, None, None, 1004);
//!
//! assert_eq!(acc.node_count(), 5);
//! assert_eq!(acc.stats().goal_count, 1);
//! assert_eq!(acc.stats().exploration_count, 2);
//! assert_eq!(acc.stats().commitment_count, 1);
//! assert_eq!(acc.stats().verification_count, 1);
//! ```

mod append;
mod convert;
mod helpers;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use super::types::{GraphEdge, GraphNode, GraphStats, NodeKind, SerializedGraph};
use crate::error::{AgentError, AgentResult};

use helpers::make_session_prefix;

// =============================================================================
// Constants
// =============================================================================

/// Filename for the persisted provenance graph within a session directory.
const GRAPH_FILENAME: &str = "graph.json";

/// Maximum number of characters for a goal node's prompt text.
const MAX_GOAL_PROMPT_LEN: usize = 500;

// =============================================================================
// ProvenanceAccumulator
// =============================================================================

/// In-memory DAG builder for a single session's provenance graph.
///
/// See the [module-level documentation](self) for details on edge inference
/// and persistence.
pub struct ProvenanceAccumulator {
    /// The session ID this graph belongs to.
    pub(crate) session_id: String,
    /// Short prefix derived from the session ID, used in node IDs.
    pub(crate) session_prefix: String,
    /// All nodes in insertion order.
    pub(crate) nodes: Vec<GraphNode>,
    /// All edges.
    pub(crate) edges: Vec<GraphEdge>,
    /// Aggregate statistics.
    pub(crate) stats: GraphStats,
    /// Monotonic counter for generating unique node IDs.
    pub(crate) counter: u64,
    /// Current goal node ID (most recently appended goal).
    pub(crate) current_goal: Option<String>,
    /// Pending exploration node IDs (cleared on commitment).
    pub(crate) pending_explorations: Vec<String>,
    /// Last commitment node ID.
    pub(crate) last_commitment: Option<String>,
    /// Last node ID (any kind).
    pub(crate) last_node: Option<String>,
    /// Pending human gate node ID (cleared on next goal).
    pub(crate) pending_human_gate: Option<String>,
    /// Commitment node IDs since the last patch proposal.
    pub(crate) commitments_since_last_patch: Vec<String>,
    /// Base32 hash of the last saved provenance artifact, if any.
    ///
    /// Used to chain per-turn provenance graphs via the `previous` field.
    pub(crate) last_provenance_hash: Option<String>,
    /// Number of nodes that have already been saved (for incremental export).
    ///
    /// `to_provenance_graph` only exports nodes added since the last save.
    pub(crate) nodes_saved_count: usize,
    /// Number of edges that have already been saved (for incremental export).
    pub(crate) edges_saved_count: usize,
}

impl ProvenanceAccumulator {
    /// Create a new empty accumulator for a session.
    pub fn new(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let session_prefix = make_session_prefix(&session_id);
        Self {
            session_id,
            session_prefix,
            nodes: Vec::new(),
            edges: Vec::new(),
            stats: GraphStats::default(),
            counter: 0,
            current_goal: None,
            pending_explorations: Vec::new(),
            last_commitment: None,
            last_node: None,
            pending_human_gate: None,
            commitments_since_last_patch: Vec::new(),
            last_provenance_hash: None,
            nodes_saved_count: 0,
            edges_saved_count: 0,
        }
    }

    /// Load from disk, or create a new empty accumulator if no persisted
    /// graph exists.
    ///
    /// The graph file is expected at `{session_dir}/graph.json`.
    pub fn load_or_create(session_dir: &Path, session_id: &str) -> AgentResult<Self> {
        let path = session_dir.join(GRAPH_FILENAME);

        match std::fs::read(&path) {
            Ok(data) => {
                let serialized: SerializedGraph =
                    serde_json::from_slice(&data).map_err(|e| AgentError::SessionLoadFailed {
                        session_id: session_id.to_string(),
                        reason: format!("provenance graph parse error: {}", e),
                    })?;
                Ok(Self::from_serialized(serialized))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new(session_id)),
            Err(e) => Err(AgentError::SessionLoadFailed {
                session_id: session_id.to_string(),
                reason: format!("provenance graph read error: {}", e),
            }),
        }
    }

    /// Reconstruct an accumulator from a serialized graph.
    fn from_serialized(s: SerializedGraph) -> Self {
        let session_prefix = make_session_prefix(&s.session_id);

        // Rebuild commitments_since_last_patch by scanning nodes from the
        // end backward to find commitment nodes that haven't been followed
        // by a patch proposal.
        let mut commitments_since_last_patch = Vec::new();
        for node in s.nodes.iter().rev() {
            if node.kind == NodeKind::PatchProposal {
                break; // Stop at the last patch proposal
            }
            if node.kind == NodeKind::Commitment {
                commitments_since_last_patch.push(node.id.clone());
            }
        }
        commitments_since_last_patch.reverse();

        let nodes_saved_count = s.nodes_saved_count.unwrap_or(s.nodes.len());
        let edges_saved_count = s.edges_saved_count.unwrap_or(s.edges.len());

        Self {
            session_id: s.session_id,
            session_prefix,
            nodes: s.nodes,
            edges: s.edges,
            stats: s.stats,
            counter: s.counter,
            current_goal: s.current_goal,
            pending_explorations: s.pending_explorations,
            last_commitment: s.last_commitment,
            last_node: s.last_node,
            pending_human_gate: s.pending_human_gate,
            commitments_since_last_patch,
            last_provenance_hash: s.last_provenance_hash,
            nodes_saved_count,
            edges_saved_count,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Session ID this graph belongs to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Aggregate statistics.
    pub fn stats(&self) -> &GraphStats {
        &self.stats
    }

    /// All nodes in insertion order.
    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    /// All edges.
    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    /// The current goal node ID, if any.
    pub fn current_goal(&self) -> Option<&str> {
        self.current_goal.as_deref()
    }

    /// Returns `true` if the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The file path where this graph would be persisted.
    pub fn graph_path(session_dir: &Path) -> PathBuf {
        session_dir.join(GRAPH_FILENAME)
    }

    /// The base32 hash of the last saved provenance artifact, if any.
    pub fn last_provenance_hash(&self) -> Option<&str> {
        self.last_provenance_hash.as_deref()
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Generate the next unique node ID and increment the counter.
    pub(crate) fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{}-{}", self.session_prefix, self.counter)
    }

    /// Push a node onto the graph, updating stats and last_node cursor.
    pub(crate) fn push_node(&mut self, node: GraphNode) {
        self.stats.increment(node.kind);
        self.last_node = Some(node.id.clone());
        self.nodes.push(node);
    }
}

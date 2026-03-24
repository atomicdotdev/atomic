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

use std::path::{Path, PathBuf};

use atomic_core::change::provenance_graph as pg;
use atomic_core::types::{Base32, Hash};

use super::classify::{classify_tool_call, summarize_tool_call};
use super::types::{EdgeKind, GraphEdge, GraphNode, GraphStats, NodeKind, SerializedGraph};
use crate::error::{AgentError, AgentResult};

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
    /// Session this graph belongs to.
    session_id: String,

    /// Short prefix of the session ID used in node IDs.
    session_prefix: String,

    /// All nodes in the graph, ordered by insertion.
    nodes: Vec<GraphNode>,

    /// All causal edges.
    edges: Vec<GraphEdge>,

    /// Aggregate statistics (kept in sync with nodes).
    stats: GraphStats,

    /// Monotonic counter for generating unique node IDs.
    counter: u64,

    // ---- Edge inference cursor state ----
    /// Most recent goal node ID.
    current_goal: Option<String>,

    /// Exploration node IDs accumulated since the last commitment.
    pending_explorations: Vec<String>,

    /// Most recent commitment node ID (for verification edges).
    last_commitment: Option<String>,

    /// Most recent node ID of any kind (for error/gate edges).
    last_node: Option<String>,

    /// Human gate node ID that is currently pending, if any.
    pending_human_gate: Option<String>,

    /// Commitment node IDs since the last patch proposal (for patch edges).
    commitments_since_last_patch: Vec<String>,

    /// Base32 hash of the last content-addressed ProvenanceGraph artifact
    /// saved for this session. Used to chain per-turn graphs via `previous`.
    last_provenance_hash: Option<String>,

    /// Number of nodes that were included in the last saved ProvenanceGraph.
    /// Used to export only the delta (new nodes) on the next save.
    /// When `to_provenance_graph()` is called, it slices `nodes[nodes_saved_count..]`.
    nodes_saved_count: usize,

    /// Number of edges that were included in the last saved ProvenanceGraph.
    edges_saved_count: usize,
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
    // Append methods
    // =========================================================================

    /// Append a goal node (human prompt).
    ///
    /// Returns the new node's ID.
    pub fn append_goal(&mut self, prompt: &str, timestamp: i64) -> String {
        let summary = truncate_prompt(prompt, MAX_GOAL_PROMPT_LEN);
        let node = GraphNode::new(self.next_id(), NodeKind::Goal, timestamp, &summary);
        let node_id = node.id.clone();

        // Edge inference: chain goals sequentially
        if let Some(ref prev_goal) = self.current_goal {
            self.edges.push(GraphEdge::new(
                prev_goal.clone(),
                node_id.clone(),
                EdgeKind::LedTo,
            ));
            self.stats.edge_count += 1;
        }

        // If there was a pending human gate, the new goal resumes after it
        if let Some(ref gate_id) = self.pending_human_gate.take() {
            self.edges.push(GraphEdge::new(
                gate_id.clone(),
                node_id.clone(),
                EdgeKind::ResumedAfter,
            ));
            self.stats.edge_count += 1;
        }

        // Reset cursor state for new goal
        self.current_goal = Some(node_id.clone());
        self.pending_explorations.clear();
        self.last_commitment = None;

        self.push_node(node);
        node_id
    }

    /// Append a tool call node, classified by the rule-based classifier.
    ///
    /// This is the primary entry point called from the orchestrator's
    /// `handle_tool_use` for `PostToolUse` events.
    ///
    /// Returns the new node's ID.
    #[allow(clippy::too_many_arguments)]
    pub fn append_tool_call(
        &mut self,
        tool_name: &str,
        tool_call_id: Option<&str>,
        tool_input: Option<&serde_json::Value>,
        tool_output: Option<&str>,
        status: Option<&str>,
        duration_ms: Option<u64>,
        timestamp: i64,
    ) -> String {
        let kind = classify_tool_call(tool_name, tool_input, tool_output, status);
        let summary = summarize_tool_call(tool_name, kind, tool_input, tool_output, status);

        let mut node =
            GraphNode::new(self.next_id(), kind, timestamp, summary).with_tool_name(tool_name);

        if let Some(id) = tool_call_id {
            node = node.with_tool_call_id(id);
        }
        if let Some(ms) = duration_ms {
            node = node.with_duration_ms(ms);
        }

        // Attach detail based on kind
        node.detail = build_tool_detail(kind, tool_name, tool_input, tool_output);

        let node_id = node.id.clone();

        // Infer edges based on the classified kind
        self.infer_edges(&node_id, kind);

        self.push_node(node);
        node_id
    }

    /// Append a reasoning/thinking node (chain-of-thought from the model).
    ///
    /// These are created from the reasoning blocks captured by the OpenCode
    /// plugin. Each block represents a distinct thinking step where the agent
    /// planned its approach, evaluated alternatives, or reasoned about the
    /// codebase.
    ///
    /// The node is classified as `Decision` (the existing kind for strategic
    /// choices). Edges link from the current goal to the reasoning node, and
    /// from the reasoning node to subsequent commitments/explorations.
    ///
    /// Returns the new node's ID.
    pub fn append_reasoning(
        &mut self,
        text: &str,
        duration_ms: Option<u64>,
        signature: Option<&str>,
        timestamp: i64,
    ) -> String {
        // Truncate for summary: first line or first 100 chars
        let first_line = text.lines().next().unwrap_or(text);
        let summary = if first_line.len() > 100 {
            let truncated: String = first_line.chars().take(97).collect();
            format!("{}...", truncated)
        } else {
            first_line.to_string()
        };

        let mut node = GraphNode::new(self.next_id(), NodeKind::Decision, timestamp, &summary);

        if let Some(ms) = duration_ms {
            node = node.with_duration_ms(ms);
        }

        // Build detail with the full reasoning text and signature
        let mut detail = serde_json::json!({
            "reasoning_text": text,
        });
        if let Some(ms) = duration_ms {
            detail["reasoning_duration_ms"] = serde_json::Value::Number(ms.into());
        }
        if let Some(sig) = signature {
            detail["anthropic_signature"] = serde_json::Value::String(sig.to_string());
        }
        detail["text_length"] = serde_json::Value::Number(text.len().into());
        node.detail = Some(detail);

        // Mark as classified so the Phase 3 consolidator doesn't touch it
        node.classified = true;
        node.confidence = Some(1.0);

        let node_id = node.id.clone();

        // Edge: goal --led_to-→ reasoning (if we have a current goal)
        if let Some(ref goal) = self.current_goal {
            self.edges.push(GraphEdge::new(
                goal.clone(),
                node_id.clone(),
                EdgeKind::LedTo,
            ));
            self.stats.edge_count += 1;
        }

        // Also chain from previous node for temporal ordering
        if let Some(ref prev) = self.last_node {
            // Only add led_to if previous wasn't already the goal
            if self.current_goal.as_ref() != Some(prev) {
                self.edges.push(GraphEdge::new(
                    prev.clone(),
                    node_id.clone(),
                    EdgeKind::LedTo,
                ));
                self.stats.edge_count += 1;
            }
        }

        self.stats.decision_count += 1;
        self.push_node(node);
        node_id
    }

    /// Append a human gate node (permission requested).
    ///
    /// Returns the new node's ID.
    pub fn append_human_gate(&mut self, reason: &str, timestamp: i64) -> String {
        let summary = truncate_prompt(reason, 200);
        let mut node = GraphNode::new(self.next_id(), NodeKind::HumanGate, timestamp, &summary);
        node.detail = Some(serde_json::json!({
            "reason": reason,
            "resolved": false,
        }));

        let node_id = node.id.clone();

        // Edge: last_node --blocked_by-→ gate
        if let Some(ref prev) = self.last_node {
            self.edges.push(GraphEdge::new(
                prev.clone(),
                node_id.clone(),
                EdgeKind::BlockedBy,
            ));
            self.stats.edge_count += 1;
        }

        self.pending_human_gate = Some(node_id.clone());

        self.push_node(node);
        node_id
    }

    /// Append a patch proposal node (change recorded).
    ///
    /// Returns the new node's ID.
    pub fn append_patch_proposal(
        &mut self,
        change_hash: &str,
        files: &[String],
        timestamp: i64,
    ) -> String {
        let file_summary = if files.is_empty() {
            String::new()
        } else if files.len() == 1 {
            files[0].clone()
        } else {
            format!("{} files", files.len())
        };

        let summary = if file_summary.is_empty() {
            format!("Change {}", short_hash(change_hash))
        } else {
            format!("Change {}: {}", short_hash(change_hash), file_summary)
        };

        let mut node = GraphNode::new(self.next_id(), NodeKind::PatchProposal, timestamp, &summary)
            .with_change_hash(change_hash);

        node.detail = Some(serde_json::json!({
            "change_hash": change_hash,
            "files": files,
        }));

        let node_id = node.id.clone();

        // Edge: each commitment since last patch --committed_via-→ this patch
        for commit_id in &self.commitments_since_last_patch {
            self.edges.push(GraphEdge::new(
                commit_id.clone(),
                node_id.clone(),
                EdgeKind::CommittedVia,
            ));
            self.stats.edge_count += 1;
        }

        // If no commitments, link from the goal
        if self.commitments_since_last_patch.is_empty() {
            if let Some(ref goal) = self.current_goal {
                self.edges.push(GraphEdge::new(
                    goal.clone(),
                    node_id.clone(),
                    EdgeKind::LedTo,
                ));
                self.stats.edge_count += 1;
            }
        }

        self.commitments_since_last_patch.clear();

        self.push_node(node);
        node_id
    }

    /// Append a raw node with full control over its fields.
    ///
    /// This method bypasses the usual edge inference logic and allows
    /// direct insertion of nodes with custom details. Used for ingesting
    /// external trace formats (e.g., Sherpa JSONL) that carry their own
    /// structured data.
    ///
    /// Returns the new node's ID.
    pub fn append_raw_node(
        &mut self,
        kind: NodeKind,
        timestamp: i64,
        summary: &str,
        detail: Option<serde_json::Value>,
    ) -> String {
        let node = GraphNode {
            id: self.next_id(),
            kind,
            timestamp,
            summary: summary.to_string(),
            detail,
            change_hash: None,
            tool_name: None,
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        };
        let node_id = node.id.clone();
        self.push_node(node);
        node_id
    }

    /// Mark a human gate as resolved.
    ///
    /// Updates the gate node's detail and clears the pending gate state.
    /// The next node appended after this will get a `ResumedAfter` edge
    /// from the goal (if any), since the gate is cleared by `append_goal`.
    pub fn resolve_human_gate(&mut self, gate_id: &str) {
        // Update the node's detail to mark it resolved
        if let Some(node) = self.nodes.iter_mut().find(|n| n.id == gate_id) {
            if let Some(ref mut detail) = node.detail {
                if let Some(obj) = detail.as_object_mut() {
                    obj.insert("resolved".into(), serde_json::Value::Bool(true));
                }
            }
        }

        // Clear pending gate if it matches
        if self.pending_human_gate.as_deref() == Some(gate_id) {
            self.pending_human_gate = None;
        }
    }

    // =========================================================================
    // Edge inference
    // =========================================================================

    /// Infer and add edges when a new tool-derived node is appended.
    fn infer_edges(&mut self, node_id: &str, kind: NodeKind) {
        match kind {
            NodeKind::Exploration => {
                // Exploration serves the current goal
                if let Some(ref goal) = self.current_goal {
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
                self.pending_explorations.push(node_id.to_string());
            }

            NodeKind::Commitment => {
                if self.pending_explorations.is_empty() {
                    // No explorations preceded this — link from goal
                    if let Some(ref goal) = self.current_goal {
                        self.edges.push(GraphEdge::new(
                            goal.clone(),
                            node_id.to_string(),
                            EdgeKind::LedTo,
                        ));
                        self.stats.edge_count += 1;
                    }
                } else {
                    // Link from each pending exploration
                    for exp_id in &self.pending_explorations {
                        self.edges.push(GraphEdge::new(
                            exp_id.clone(),
                            node_id.to_string(),
                            EdgeKind::ExploredVia,
                        ));
                        self.stats.edge_count += 1;
                    }
                    self.pending_explorations.clear();
                }

                self.last_commitment = Some(node_id.to_string());
                self.commitments_since_last_patch.push(node_id.to_string());
            }

            NodeKind::Verification => {
                // Verification validates the most recent commitment
                if let Some(ref commit) = self.last_commitment {
                    self.edges.push(GraphEdge::new(
                        commit.clone(),
                        node_id.to_string(),
                        EdgeKind::VerifiedBy,
                    ));
                    self.stats.edge_count += 1;
                } else if let Some(ref goal) = self.current_goal {
                    // No commitment to verify — link from goal
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            NodeKind::Execution => {
                // Execution serves the current goal
                if let Some(ref goal) = self.current_goal {
                    self.edges.push(GraphEdge::new(
                        goal.clone(),
                        node_id.to_string(),
                        EdgeKind::LedTo,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            NodeKind::Error => {
                // Error caused by whatever preceded it
                if let Some(ref prev) = self.last_node {
                    self.edges.push(GraphEdge::new(
                        prev.clone(),
                        node_id.to_string(),
                        EdgeKind::FailedWith,
                    ));
                    self.stats.edge_count += 1;
                }
            }

            // Goal, HumanGate, PatchProposal handle their own edges in
            // their append_* methods. Decision nodes are created by the
            // Phase 3 classifier, not by append_tool_call.
            _ => {}
        }
    }

    // =========================================================================
    // Consolidation
    // =========================================================================

    /// Consolidate raw tool nodes into Decision nodes.
    ///
    /// Scans the graph for recognizable sequences of unclassified tool nodes
    /// and collapses each into a single `Decision` node. Original nodes are
    /// preserved and marked `classified = true`. Idempotent — running twice
    /// produces the same result.
    ///
    /// Returns the number of decision nodes created.
    pub fn consolidate(&mut self) -> u32 {
        super::consolidate::consolidate(
            &mut self.nodes,
            &mut self.edges,
            &mut self.stats,
            &mut self.counter,
            &self.session_prefix,
        )
    }

    // =========================================================================
    // Conversion to content-addressed ProvenanceGraph
    // =========================================================================

    /// Convert the accumulated graph to a content-addressed `ProvenanceGraph`
    /// suitable for storage in the Atomic graph alongside changes and attestations.
    ///
    /// **Per-turn delta**: Only includes nodes and edges added since the last
    /// save (tracked by `nodes_saved_count` / `edges_saved_count`). Each
    /// turn's provenance graph is self-contained — the `previous` field links
    /// to the prior turn's graph for historical context.
    ///
    /// The `previous` field is automatically set from `last_provenance_hash`
    /// if a prior graph was saved for this session. Call
    /// [`Self::set_last_provenance_hash`] after saving to maintain the chain.
    pub fn to_provenance_graph(
        &mut self,
        agent_name: &str,
        agent_display_name: &str,
        agent_vendor: &str,
        changes_explained: &[Hash],
    ) -> pg::ProvenanceGraph {
        let previous = self
            .last_provenance_hash
            .as_ref()
            .and_then(|s| Hash::from_base32(s.as_bytes()));

        // Only export nodes/edges added since the last save (per-turn delta).
        let new_nodes = &self.nodes[self.nodes_saved_count..];
        let new_edges = &self.edges[self.edges_saved_count..];

        // Collect IDs of new nodes for edge filtering
        let new_node_ids: std::collections::HashSet<&str> =
            new_nodes.iter().map(|n| n.id.as_str()).collect();

        // Only include edges where BOTH endpoints are in the new node set.
        // Cross-turn edges (e.g., goal from turn 1 → exploration in turn 2)
        // are dropped — each turn's graph is self-contained.
        let relevant_edges: Vec<&GraphEdge> = new_edges
            .iter()
            .filter(|e| {
                new_node_ids.contains(e.from.as_str()) || new_node_ids.contains(e.to.as_str())
            })
            .collect();

        let nodes: Vec<pg::ProvenanceNode> = new_nodes
            .iter()
            .map(|n| pg::ProvenanceNode {
                id: n.id.clone(),
                kind: convert_node_kind(n.kind),
                timestamp: n.timestamp,
                summary: n.summary.clone(),
                detail: n.detail.as_ref().map(|d| d.to_string()),
                change_hash: n
                    .change_hash
                    .as_ref()
                    .and_then(|s| Hash::from_base32(s.as_bytes())),
                tool_name: n.tool_name.clone(),
                tool_call_id: n.tool_call_id.clone(),
                duration_ms: n.duration_ms,
                classified: n.classified,
                confidence: n.confidence,
                consolidated_from: n.consolidated_from.clone(),
            })
            .collect();

        let edges: Vec<pg::ProvenanceEdge> = relevant_edges
            .iter()
            .map(|e| pg::ProvenanceEdge {
                from: e.from.clone(),
                to: e.to.clone(),
                kind: convert_edge_kind(e.kind),
            })
            .collect();

        // Mark the save point so the next call only exports new nodes
        self.nodes_saved_count = self.nodes.len();
        self.edges_saved_count = self.edges.len();

        let mut builder = pg::ProvenanceGraph::builder(&self.session_id, agent_name)
            .agent_display_name(agent_display_name)
            .agent_vendor(agent_vendor)
            .nodes(nodes)
            .edges(edges)
            .changes_explained(changes_explained.to_vec());

        if let Some(prev) = previous {
            builder = builder.previous(prev);
        }

        builder.build()
    }

    /// Record the hash of a saved ProvenanceGraph artifact so subsequent
    /// graphs chain to it via `previous`.
    ///
    /// Call this after `Repository::save_provenance_graph()` succeeds,
    /// then call [`Self::save`] to persist the updated state.
    pub fn set_last_provenance_hash(&mut self, hash_base32: impl Into<String>) {
        self.last_provenance_hash = Some(hash_base32.into());
    }

    // =========================================================================
    // Serialization
    // =========================================================================

    /// Persist the graph to disk at `{session_dir}/graph.json`.
    ///
    /// Uses atomic write (temp file + rename) to prevent corruption.
    pub fn save(&self, session_dir: &Path) -> AgentResult<()> {
        // Ensure the session directory exists
        std::fs::create_dir_all(session_dir).map_err(|e| AgentError::SessionSaveFailed {
            session_id: self.session_id.clone(),
            reason: format!("create session dir: {}", e),
        })?;

        let path = session_dir.join(GRAPH_FILENAME);
        let tmp_path = path.with_extension("json.tmp");

        let serialized = self.to_serialized_graph();
        let data = serde_json::to_string_pretty(&serialized).map_err(|e| {
            AgentError::SessionSaveFailed {
                session_id: self.session_id.clone(),
                reason: format!("provenance graph serialize: {}", e),
            }
        })?;

        // Write to temp file
        std::fs::write(&tmp_path, data.as_bytes()).map_err(|e| AgentError::SessionSaveFailed {
            session_id: self.session_id.clone(),
            reason: format!("provenance graph write temp: {}", e),
        })?;

        // Atomic rename
        std::fs::rename(&tmp_path, &path).map_err(|e| AgentError::SessionSaveFailed {
            session_id: self.session_id.clone(),
            reason: format!("provenance graph rename: {}", e),
        })?;

        Ok(())
    }

    /// Serialize to the full JSON-compatible representation.
    pub fn to_serialized_graph(&self) -> SerializedGraph {
        SerializedGraph {
            version: SerializedGraph::VERSION,
            session_id: self.session_id.clone(),
            created_at: chrono::Utc::now().timestamp_millis(),
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            stats: self.stats.clone(),
            counter: self.counter,
            current_goal: self.current_goal.clone(),
            pending_explorations: self.pending_explorations.clone(),
            last_commitment: self.last_commitment.clone(),
            last_node: self.last_node.clone(),
            pending_human_gate: self.pending_human_gate.clone(),
            last_provenance_hash: self.last_provenance_hash.clone(),
            nodes_saved_count: Some(self.nodes_saved_count),
            edges_saved_count: Some(self.edges_saved_count),
        }
    }

    /// Serialize to a compact text summary for LLM compaction context.
    ///
    /// The summary is structured but concise, optimized for token budget:
    /// - Lists goals (one line each)
    /// - Shows the decision chain (explorations → commitments → verifications)
    /// - Lists recorded patches with change hashes
    /// - Lists pending human gates
    ///
    /// Skips raw exploration/verification details to keep the summary tight.
    /// Targets <500 tokens for a typical 20-node session.
    pub fn to_compaction_summary(&self) -> String {
        let mut lines = Vec::new();

        let total = self.node_count();
        lines.push(format!("## Session Provenance ({} nodes)", total));
        lines.push(String::new());

        // Goals
        let goals: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Goal)
            .collect();
        if !goals.is_empty() {
            lines.push("### Goals".to_string());
            for g in &goals {
                lines.push(format!("- {}", g.summary));
            }
            lines.push(String::new());
        }

        // Decision chain: group by goal
        let decisions: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Decision)
            .collect();
        if !decisions.is_empty() {
            lines.push("### Decisions".to_string());
            for d in &decisions {
                lines.push(format!("- {}", d.summary));
            }
            lines.push(String::new());
        }

        // If no consolidated decisions yet, show commitment summary
        if decisions.is_empty() {
            let commitments: Vec<&GraphNode> = self
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Commitment)
                .collect();
            if !commitments.is_empty() {
                lines.push("### Changes Made".to_string());
                for c in &commitments {
                    lines.push(format!("- {}", c.summary));
                }
                lines.push(String::new());
            }
        }

        // Verifications summary (just count + last result)
        let verifications: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Verification)
            .collect();
        if !verifications.is_empty() {
            lines.push("### Verifications".to_string());
            for v in &verifications {
                lines.push(format!("- {}", v.summary));
            }
            lines.push(String::new());
        }

        // Patches
        let patches: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::PatchProposal)
            .collect();
        if !patches.is_empty() {
            lines.push("### Recorded Changes".to_string());
            for p in &patches {
                lines.push(format!("- {}", p.summary));
            }
            lines.push(String::new());
        }

        // Human gates
        let gates: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::HumanGate)
            .collect();
        if !gates.is_empty() {
            lines.push("### Human Gates".to_string());
            for g in &gates {
                let resolved = g
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("resolved"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let status = if resolved { "resolved" } else { "pending" };
                lines.push(format!("- {} ({})", g.summary, status));
            }
            lines.push(String::new());
        }

        // Errors
        let errors: Vec<&GraphNode> = self
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Error)
            .collect();
        if !errors.is_empty() {
            lines.push("### Errors".to_string());
            for e in &errors {
                lines.push(format!("- {}", e.summary));
            }
            lines.push(String::new());
        }

        // Trim trailing empty line
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }

        lines.join("\n")
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

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Generate the next unique node ID and increment the counter.
    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{}-{}", self.session_prefix, self.counter)
    }

    /// Push a node onto the graph, updating stats and last_node cursor.
    fn push_node(&mut self, node: GraphNode) {
        self.stats.increment(node.kind);
        self.last_node = Some(node.id.clone());
        self.nodes.push(node);
    }

    /// The base32 hash of the last saved provenance artifact, if any.
    pub fn last_provenance_hash(&self) -> Option<&str> {
        self.last_provenance_hash.as_deref()
    }
}

// =============================================================================
// Kind conversion helpers
// =============================================================================

/// Convert from agent-side `NodeKind` to core `ProvenanceNodeKind`.
fn convert_node_kind(kind: NodeKind) -> pg::ProvenanceNodeKind {
    match kind {
        NodeKind::Goal => pg::ProvenanceNodeKind::Goal,
        NodeKind::Exploration => pg::ProvenanceNodeKind::Exploration,
        NodeKind::Decision => pg::ProvenanceNodeKind::Decision,
        NodeKind::Commitment => pg::ProvenanceNodeKind::Commitment,
        NodeKind::Verification => pg::ProvenanceNodeKind::Verification,
        NodeKind::Execution => pg::ProvenanceNodeKind::Execution,
        NodeKind::HumanGate => pg::ProvenanceNodeKind::HumanGate,
        NodeKind::PatchProposal => pg::ProvenanceNodeKind::PatchProposal,
        NodeKind::Error => pg::ProvenanceNodeKind::Error,
        NodeKind::Todo => pg::ProvenanceNodeKind::Todo,
        NodeKind::TodoStatusChange => pg::ProvenanceNodeKind::TodoStatusChange,
        NodeKind::PhaseTransition => pg::ProvenanceNodeKind::PhaseTransition,
        NodeKind::Lesson => pg::ProvenanceNodeKind::Lesson,
        NodeKind::LlmResponse => pg::ProvenanceNodeKind::LlmResponse,
        NodeKind::HumanGateResolution => pg::ProvenanceNodeKind::HumanGateResolution,
    }
}

/// Convert from agent-side `EdgeKind` to core `ProvenanceEdgeKind`.
fn convert_edge_kind(kind: EdgeKind) -> pg::ProvenanceEdgeKind {
    match kind {
        EdgeKind::LedTo => pg::ProvenanceEdgeKind::LedTo,
        EdgeKind::ExploredVia => pg::ProvenanceEdgeKind::ExploredVia,
        EdgeKind::CommittedVia => pg::ProvenanceEdgeKind::CommittedVia,
        EdgeKind::VerifiedBy => pg::ProvenanceEdgeKind::VerifiedBy,
        EdgeKind::BlockedBy => pg::ProvenanceEdgeKind::BlockedBy,
        EdgeKind::ResumedAfter => pg::ProvenanceEdgeKind::ResumedAfter,
        EdgeKind::FailedWith => pg::ProvenanceEdgeKind::FailedWith,
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Create a short prefix from a session ID for use in node IDs.
///
/// Takes up to the first 8 characters. If the session ID is a UUID,
/// this is the first segment before the first hyphen (or the first 8
/// chars, whichever is shorter).
fn make_session_prefix(session_id: &str) -> String {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return "s".to_string();
    }
    // Take first segment before hyphen, capped at 8 chars
    let segment = trimmed.split('-').next().unwrap_or(trimmed);
    let capped: String = segment.chars().take(8).collect();
    if capped.is_empty() {
        "s".to_string()
    } else {
        capped
    }
}

/// Truncate a prompt string for display, preserving word boundaries where possible.
fn truncate_prompt(prompt: &str, max_len: usize) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }

    // Try to break at a word boundary
    let truncated = &trimmed[..max_len.saturating_sub(3)];
    if let Some(last_space) = truncated.rfind(' ') {
        if last_space > max_len / 2 {
            return format!("{}...", &truncated[..last_space]);
        }
    }

    format!("{}...", truncated)
}

/// Shorten a hash for display (first 8 characters).
fn short_hash(hash: &str) -> &str {
    if hash.len() > 8 {
        &hash[..8]
    } else {
        hash
    }
}

/// Build the `detail` JSON for a tool-derived node.
fn build_tool_detail(
    kind: NodeKind,
    tool_name: &str,
    tool_input: Option<&serde_json::Value>,
    tool_output: Option<&str>,
) -> Option<serde_json::Value> {
    match kind {
        NodeKind::Exploration => {
            let path = tool_input
                .and_then(|v| {
                    v.get("path")
                        .or_else(|| v.get("file"))
                        .or_else(|| v.get("glob"))
                        .or_else(|| v.get("regex"))
                })
                .and_then(|v| v.as_str());
            path.map(|p| serde_json::json!({"tool": tool_name, "target": p}))
        }

        NodeKind::Commitment => {
            // Extract file path from tool_input — OpenCode uses "filePath" (camelCase)
            // while other agents may use "path", "file", or "file_path" (snake_case).
            // Also check the top-level "file_path" field added by the enriched plugin.
            let path = tool_input
                .and_then(|v| {
                    v.get("filePath")
                        .or_else(|| v.get("path"))
                        .or_else(|| v.get("file"))
                        .or_else(|| v.get("file_path"))
                })
                .and_then(|v| v.as_str());

            let mut detail = serde_json::json!({"tool": tool_name});

            if let Some(p) = path {
                // Store both the full path and a shortened display path
                detail["file_path"] = serde_json::Value::String(p.to_string());
                // Shorten: take last 2-3 path components for display
                let short = shorten_path(p);
                detail["file"] = serde_json::Value::String(short);
            }

            // Determine operation: create vs edit
            // "write" tool = create (new file), "edit"/"multiedit"/"patch" = edit
            let operation = match tool_name.to_lowercase().as_str() {
                "write" | "write_file" | "create" | "create_file" => "create",
                "delete_file" | "remove_file" => "delete",
                _ => "edit",
            };
            detail["operation"] = serde_json::Value::String(operation.to_string());

            // Pull in filediff from the enriched after-tool payload if present.
            // The plugin sends: { filediff: { file, before, after, additions, deletions } }
            if let Some(filediff) = tool_input.and_then(|v| v.get("filediff")).cloned() {
                detail["filediff"] = filediff;
            }

            // Pull in unified diff string
            if let Some(diff) = tool_input
                .and_then(|v| v.get("diff"))
                .and_then(|v| v.as_str())
            {
                detail["diff"] = serde_json::Value::String(diff.to_string());
            }

            // Pull in diagnostics from the enriched after-tool payload
            if let Some(diag) = tool_input.and_then(|v| v.get("diagnostics")).cloned() {
                detail["diagnostics"] = diag;
            }

            // Pull in title (human-readable description)
            if let Some(title) = tool_input
                .and_then(|v| v.get("title"))
                .and_then(|v| v.as_str())
            {
                detail["title"] = serde_json::Value::String(title.to_string());
            }

            // Check if the file existed before (write to new file vs overwrite)
            if let Some(exists) = tool_input
                .and_then(|v| v.get("exists"))
                .and_then(|v| v.as_bool())
            {
                detail["exists"] = serde_json::Value::Bool(exists);
                if !exists {
                    detail["operation"] = serde_json::Value::String("create".to_string());
                }
            }

            Some(detail)
        }

        NodeKind::Verification => {
            let cmd = tool_input
                .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
                .and_then(|v| v.as_str());
            let description = tool_input
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str());
            let exit_code = tool_input
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64());
            let mut detail = serde_json::json!({});
            if let Some(c) = cmd {
                detail["command"] = serde_json::Value::String(c.to_string());
            }
            if let Some(d) = description {
                detail["description"] = serde_json::Value::String(d.to_string());
            }
            if let Some(code) = exit_code {
                detail["exit_code"] = serde_json::Value::Number(code.into());
            }
            // Try to determine pass/fail from exit code first, then output heuristics
            if let Some(code) = exit_code {
                detail["passed"] = serde_json::Value::Bool(code == 0);
            } else if let Some(output) = tool_output {
                let lower = output.to_lowercase();
                if lower.contains("fail") || lower.contains("error") || lower.contains("failed") {
                    detail["passed"] = serde_json::Value::Bool(false);
                } else if lower.contains("pass")
                    || lower.contains("ok")
                    || lower.contains("success")
                {
                    detail["passed"] = serde_json::Value::Bool(true);
                }
            }
            // Pull in diagnostics if present
            if let Some(diag) = tool_input.and_then(|v| v.get("diagnostics")).cloned() {
                detail["diagnostics"] = diag;
            }
            // Truncated output summary
            if let Some(output) = tool_output {
                let truncated: String = output.chars().take(300).collect();
                detail["output_summary"] = serde_json::Value::String(truncated);
            }
            Some(detail)
        }

        NodeKind::Execution => {
            let cmd = tool_input
                .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
                .and_then(|v| v.as_str());
            let description = tool_input
                .and_then(|v| v.get("description"))
                .and_then(|v| v.as_str());
            let exit_code = tool_input
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64());
            let mut detail = serde_json::json!({});
            if let Some(c) = cmd {
                detail["command"] = serde_json::Value::String(c.to_string());
            }
            if let Some(d) = description {
                detail["description"] = serde_json::Value::String(d.to_string());
            }
            if let Some(code) = exit_code {
                detail["exit_code"] = serde_json::Value::Number(code.into());
            }
            // Truncated output summary
            if let Some(output) = tool_output {
                let truncated: String = output.chars().take(300).collect();
                detail["output_summary"] = serde_json::Value::String(truncated);
            }
            if detail.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                None
            } else {
                Some(detail)
            }
        }

        NodeKind::Error => {
            let mut detail = serde_json::json!({"tool": tool_name});
            if let Some(output) = tool_output {
                // Truncate error output to keep detail manageable
                let truncated: String = output.chars().take(500).collect();
                detail["error"] = serde_json::Value::String(truncated);
            }
            Some(detail)
        }

        _ => None,
    }
}

/// Shorten a file path to the last 2-3 components for display.
///
/// `/Users/leefaus/Projects/hello-world/src/index.ts` → `src/index.ts`
/// `/Users/leefaus/Projects/hello-world/tsconfig.json` → `tsconfig.json`
fn shorten_path(full_path: &str) -> String {
    let parts: Vec<&str> = full_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return parts.join("/");
    }

    // Look for common project markers to find the project-relative path
    let markers = ["src", "lib", "app", "test", "tests", "dist", "pkg", "cmd"];
    for (i, part) in parts.iter().enumerate() {
        if markers.contains(&part.to_lowercase().as_str()) {
            return parts[i..].join("/");
        }
    }

    // Fall back to the last 2 segments
    parts[parts.len() - 2..].join("/")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Construction ----

    #[test]
    fn test_new_accumulator_is_empty() {
        let acc = ProvenanceAccumulator::new("test-session");
        assert_eq!(acc.session_id(), "test-session");
        assert!(acc.is_empty());
        assert_eq!(acc.node_count(), 0);
        assert_eq!(acc.edge_count(), 0);
        assert!(acc.stats().is_empty());
        assert!(acc.current_goal().is_none());
    }

    // ---- append_goal ----

    #[test]
    fn test_append_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let id = acc.append_goal("Fix the auth bug", 1000);

        assert!(!id.is_empty());
        assert_eq!(acc.node_count(), 1);
        assert_eq!(acc.stats().goal_count, 1);
        assert_eq!(acc.current_goal(), Some(id.as_str()));

        let node = &acc.nodes()[0];
        assert_eq!(node.kind, NodeKind::Goal);
        assert_eq!(node.summary, "Fix the auth bug");
        assert_eq!(node.timestamp, 1000);
    }

    #[test]
    fn test_chained_goals() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let g1 = acc.append_goal("First goal", 1000);
        let g2 = acc.append_goal("Second goal", 2000);

        assert_eq!(acc.stats().goal_count, 2);
        assert_eq!(acc.current_goal(), Some(g2.as_str()));

        // Should have a led_to edge from g1 → g2
        assert_eq!(acc.edge_count(), 1);
        let edge = &acc.edges()[0];
        assert_eq!(edge.from, g1);
        assert_eq!(edge.to, g2);
        assert_eq!(edge.kind, EdgeKind::LedTo);
    }

    #[test]
    fn test_goal_resets_pending_explorations() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _g1 = acc.append_goal("First", 1000);
        let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let _r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);

        // pending_explorations should have 2 entries
        assert_eq!(acc.pending_explorations.len(), 2);

        // New goal resets them
        let _g2 = acc.append_goal("Second", 2000);
        assert_eq!(acc.pending_explorations.len(), 0);
    }

    // ---- append_tool_call: exploration ----

    #[test]
    fn test_append_exploration() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Fix bug", 1000);
        let read_id = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);

        assert_eq!(acc.node_count(), 2);
        assert_eq!(acc.stats().exploration_count, 1);

        // Should have edge: goal --led_to-→ read
        let edge = acc
            .edges()
            .iter()
            .find(|e| e.from == goal && e.to == read_id)
            .expect("should have goal → exploration edge");
        assert_eq!(edge.kind, EdgeKind::LedTo);
    }

    #[test]
    fn test_multiple_explorations_all_link_to_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Fix bug", 1000);
        let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let r2 = acc.append_tool_call("grep", Some("c2"), None, None, None, None, 1002);
        let r3 = acc.append_tool_call("list_directory", Some("c3"), None, None, None, None, 1003);

        assert_eq!(acc.stats().exploration_count, 3);

        // All explorations should have led_to edges from goal
        for rid in [&r1, &r2, &r3] {
            assert!(
                acc.edges()
                    .iter()
                    .any(|e| e.from == goal && e.to == *rid && e.kind == EdgeKind::LedTo),
                "goal should lead to exploration {}",
                rid
            );
        }
    }

    // ---- append_tool_call: commitment ----

    #[test]
    fn test_commitment_with_preceding_explorations() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _goal = acc.append_goal("Fix bug", 1000);
        let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
        let edit = acc.append_tool_call("edit", Some("c3"), None, None, None, None, 1003);

        assert_eq!(acc.stats().commitment_count, 1);

        // Explorations → commitment via explored_via
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == r1 && e.to == edit && e.kind == EdgeKind::ExploredVia));
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == r2 && e.to == edit && e.kind == EdgeKind::ExploredVia));

        // Pending explorations should be cleared
        assert!(acc.pending_explorations.is_empty());
    }

    #[test]
    fn test_commitment_without_explorations_links_to_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Fix bug", 1000);
        let edit = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);

        // Should link directly from goal
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == goal && e.to == edit && e.kind == EdgeKind::LedTo));
    }

    // ---- append_tool_call: verification ----

    #[test]
    fn test_verification_links_to_commitment() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _goal = acc.append_goal("Fix bug", 1000);
        let edit = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);

        let test_input = serde_json::json!({"command": "cargo test"});
        let test = acc.append_tool_call(
            "bash",
            Some("c2"),
            Some(&test_input),
            None,
            None,
            None,
            1002,
        );

        assert_eq!(acc.stats().verification_count, 1);

        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == edit && e.to == test && e.kind == EdgeKind::VerifiedBy));
    }

    #[test]
    fn test_verification_without_commitment_links_to_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Run tests", 1000);

        let test_input = serde_json::json!({"command": "cargo test"});
        let test = acc.append_tool_call(
            "bash",
            Some("c1"),
            Some(&test_input),
            None,
            None,
            None,
            1001,
        );

        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == goal && e.to == test && e.kind == EdgeKind::LedTo));
    }

    // ---- append_tool_call: execution ----

    #[test]
    fn test_execution_links_to_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Setup", 1000);

        let install_input = serde_json::json!({"command": "npm install express"});
        let install = acc.append_tool_call(
            "bash",
            Some("c1"),
            Some(&install_input),
            None,
            None,
            None,
            1001,
        );

        assert_eq!(acc.stats().execution_count, 1);
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == goal && e.to == install && e.kind == EdgeKind::LedTo));
    }

    // ---- append_tool_call: error ----

    #[test]
    fn test_error_links_to_last_node() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _goal = acc.append_goal("Fix bug", 1000);
        let read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let error = acc.append_tool_call("edit", Some("c2"), None, None, Some("error"), None, 1002);

        assert_eq!(acc.stats().error_count, 1);
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == read && e.to == error && e.kind == EdgeKind::FailedWith));
    }

    // ---- append_human_gate ----

    #[test]
    fn test_human_gate() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _goal = acc.append_goal("Fix bug", 1000);
        let read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let gate = acc.append_human_gate("Delete old tokens?", 1002);

        assert_eq!(acc.stats().human_gate_count, 1);
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == read && e.to == gate && e.kind == EdgeKind::BlockedBy));
        assert_eq!(acc.pending_human_gate.as_deref(), Some(gate.as_str()));
    }

    #[test]
    fn test_resolve_human_gate() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let gate = acc.append_human_gate("Delete old tokens?", 1000);

        acc.resolve_human_gate(&gate);

        let node = acc.nodes().iter().find(|n| n.id == gate).unwrap();
        let resolved = node
            .detail
            .as_ref()
            .unwrap()
            .get("resolved")
            .unwrap()
            .as_bool()
            .unwrap();
        assert!(resolved);
        assert!(acc.pending_human_gate.is_none());
    }

    #[test]
    fn test_goal_after_gate_has_resumed_edge() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let gate = acc.append_human_gate("Should I proceed?", 1000);
        let goal = acc.append_goal("Yes, proceed", 1001);

        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == gate && e.to == goal && e.kind == EdgeKind::ResumedAfter));
    }

    // ---- append_patch_proposal ----

    #[test]
    fn test_patch_proposal_links_to_commitments() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let _goal = acc.append_goal("Fix bug", 1000);
        let edit1 = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);
        let edit2 = acc.append_tool_call("write", Some("c2"), None, None, None, None, 1002);
        let patch = acc.append_patch_proposal(
            "ABCD1234EFGH5678",
            &["src/a.rs".into(), "src/b.rs".into()],
            1003,
        );

        assert_eq!(acc.stats().patch_proposal_count, 1);

        // Both commits → patch via committed_via
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == edit1 && e.to == patch && e.kind == EdgeKind::CommittedVia));
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == edit2 && e.to == patch && e.kind == EdgeKind::CommittedVia));

        // commitments_since_last_patch should be cleared
        assert!(acc.commitments_since_last_patch.is_empty());
    }

    #[test]
    fn test_patch_proposal_without_commitments_links_to_goal() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let goal = acc.append_goal("Fix bug", 1000);
        let patch = acc.append_patch_proposal("ABCD", &[], 1001);

        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == goal && e.to == patch && e.kind == EdgeKind::LedTo));
    }

    #[test]
    fn test_patch_proposal_display_single_file() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_patch_proposal("ABCDEF123456", &["src/main.rs".into()], 1000);

        let node = &acc.nodes()[0];
        assert!(node.summary.contains("ABCDEF12"));
        assert!(node.summary.contains("src/main.rs"));
    }

    #[test]
    fn test_patch_proposal_display_multiple_files() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_patch_proposal(
            "HASH1234",
            &["a.rs".into(), "b.rs".into(), "c.rs".into()],
            1000,
        );

        let node = &acc.nodes()[0];
        assert!(node.summary.contains("3 files"));
    }

    // ---- Full session flow ----

    #[test]
    fn test_typical_session_graph_structure() {
        let mut acc = ProvenanceAccumulator::new("test-session");

        // Human asks to fix a bug
        let goal = acc.append_goal("Fix the auth bug", 1000);

        // Agent reads 3 files
        let r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let r2 = acc.append_tool_call("read", Some("c2"), None, None, None, None, 1002);
        let r3 = acc.append_tool_call("read", Some("c3"), None, None, None, None, 1003);

        // Agent edits one file
        let edit = acc.append_tool_call("edit", Some("c4"), None, None, None, None, 1004);

        // Agent runs tests
        let test_input = serde_json::json!({"command": "cargo test"});
        let test = acc.append_tool_call(
            "bash",
            Some("c5"),
            Some(&test_input),
            None,
            None,
            None,
            1005,
        );

        // Verify node counts
        assert_eq!(acc.node_count(), 6);
        assert_eq!(acc.stats().goal_count, 1);
        assert_eq!(acc.stats().exploration_count, 3);
        assert_eq!(acc.stats().commitment_count, 1);
        assert_eq!(acc.stats().verification_count, 1);

        // Verify edges
        // goal → r1, r2, r3 (led_to)
        for r in [&r1, &r2, &r3] {
            assert!(
                acc.edges()
                    .iter()
                    .any(|e| e.from == goal && e.to == *r && e.kind == EdgeKind::LedTo),
                "goal → {} led_to",
                r
            );
        }

        // r1, r2, r3 → edit (explored_via)
        for r in [&r1, &r2, &r3] {
            assert!(
                acc.edges()
                    .iter()
                    .any(|e| e.from == *r && e.to == edit && e.kind == EdgeKind::ExploredVia),
                "{} → edit explored_via",
                r
            );
        }

        // edit → test (verified_by)
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == edit && e.to == test && e.kind == EdgeKind::VerifiedBy));
    }

    #[test]
    fn test_multi_turn_session() {
        let mut acc = ProvenanceAccumulator::new("s1");

        // Turn 1: fix a bug
        let g1 = acc.append_goal("Fix auth bug", 1000);
        let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let e1 = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);
        let _p1 = acc.append_patch_proposal("HASH1", &["auth.rs".into()], 1003);

        // Turn 2: add tests
        let g2 = acc.append_goal("Add tests", 2000);
        let _r2 = acc.append_tool_call("read", Some("c3"), None, None, None, None, 2001);
        let e2 = acc.append_tool_call("write", Some("c4"), None, None, None, None, 2002);

        let test_input = serde_json::json!({"command": "cargo test"});
        let _t2 = acc.append_tool_call(
            "bash",
            Some("c5"),
            Some(&test_input),
            None,
            None,
            None,
            2003,
        );
        let _p2 = acc.append_patch_proposal("HASH2", &["test_auth.rs".into()], 2004);

        assert_eq!(acc.stats().goal_count, 2);
        assert_eq!(acc.stats().patch_proposal_count, 2);

        // g1 → g2 chained
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == g1 && e.to == g2 && e.kind == EdgeKind::LedTo));

        // First patch only linked to first edit
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == e1 && e.kind == EdgeKind::CommittedVia));
        // Second patch linked to second edit, not first
        assert!(acc
            .edges()
            .iter()
            .any(|e| e.from == e2 && e.kind == EdgeKind::CommittedVia));
    }

    // ---- Serialization round-trip ----

    #[test]
    fn test_serialized_graph_roundtrip() {
        let mut acc = ProvenanceAccumulator::new("roundtrip-test");
        acc.append_goal("Fix bug", 1000);
        acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

        let serialized = acc.to_serialized_graph();
        let json = serde_json::to_string_pretty(&serialized).unwrap();
        let deserialized: SerializedGraph = serde_json::from_str(&json).unwrap();

        let restored = ProvenanceAccumulator::from_serialized(deserialized);

        assert_eq!(restored.node_count(), acc.node_count());
        assert_eq!(restored.edge_count(), acc.edge_count());
        assert_eq!(restored.session_id(), acc.session_id());
        assert_eq!(restored.current_goal(), acc.current_goal());
        assert_eq!(restored.counter, acc.counter);
    }

    #[test]
    fn test_serialization_preserves_accumulator_state() {
        let mut acc = ProvenanceAccumulator::new("state-test");
        acc.append_goal("Goal", 1000);
        let _r1 = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

        let serialized = acc.to_serialized_graph();

        // Accumulator state should be preserved
        assert!(serialized.current_goal.is_some());
        // After the edit, pending explorations should be cleared
        assert!(serialized.pending_explorations.is_empty());
        assert!(serialized.last_commitment.is_some());
        assert!(serialized.last_node.is_some());
    }

    #[test]
    fn test_from_serialized_rebuilds_commitments_since_patch() {
        let mut acc = ProvenanceAccumulator::new("rebuild-test");
        acc.append_goal("Fix", 1000);
        let e1 = acc.append_tool_call("edit", Some("c1"), None, None, None, None, 1001);
        acc.append_patch_proposal("HASH1", &[], 1002);
        let e2 = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1003);
        let e3 = acc.append_tool_call("write", Some("c3"), None, None, None, None, 1004);

        let serialized = acc.to_serialized_graph();
        let restored = ProvenanceAccumulator::from_serialized(serialized);

        // Should only have e2 and e3, not e1 (which was before the patch)
        assert_eq!(restored.commitments_since_last_patch.len(), 2);
        assert!(restored.commitments_since_last_patch.contains(&e2));
        assert!(restored.commitments_since_last_patch.contains(&e3));
        assert!(!restored.commitments_since_last_patch.contains(&e1));
    }

    // ---- to_provenance_graph conversion ----

    #[test]
    fn test_to_provenance_graph_basic() {
        let mut acc = ProvenanceAccumulator::new("sess-convert");
        acc.append_goal("Fix the auth bug", 1000);
        acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

        let change_hash = Hash::of(b"test-change");
        let graph =
            acc.to_provenance_graph("claude-code", "Claude Code", "anthropic", &[change_hash]);

        assert_eq!(graph.session_id, "sess-convert");
        assert_eq!(graph.agent_name, "claude-code");
        assert_eq!(graph.agent_display_name, "Claude Code");
        assert_eq!(graph.agent_vendor, "anthropic");
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), acc.edge_count());
        assert_eq!(graph.changes_explained, vec![change_hash]);
        assert!(graph.previous.is_none());

        // Verify node kinds were converted correctly
        assert_eq!(graph.nodes[0].kind, pg::ProvenanceNodeKind::Goal);
        assert_eq!(graph.nodes[1].kind, pg::ProvenanceNodeKind::Exploration);
        assert_eq!(graph.nodes[2].kind, pg::ProvenanceNodeKind::Commitment);

        // Verify stats were auto-computed
        assert_eq!(graph.stats.goal_count, 1);
        assert_eq!(graph.stats.exploration_count, 1);
        assert_eq!(graph.stats.commitment_count, 1);
    }

    #[test]
    fn test_to_provenance_graph_with_chaining() {
        let mut acc = ProvenanceAccumulator::new("sess-chain");
        acc.append_goal("Second turn", 2000);

        let prev_hash = Hash::of(b"previous-graph");
        acc.set_last_provenance_hash(prev_hash.to_base32());
        let graph = acc.to_provenance_graph("opencode", "OpenCode", "anthropic", &[]);

        assert!(graph.is_chained());
        assert_eq!(graph.previous, Some(prev_hash));
    }

    #[test]
    fn test_to_provenance_graph_serializes_cleanly() {
        let mut acc = ProvenanceAccumulator::new("sess-serialize");
        acc.append_goal("Fix bug", 1000);

        let input = serde_json::json!({"path": "src/auth.rs"});
        acc.append_tool_call("read", Some("c1"), Some(&input), None, None, None, 1001);
        acc.append_tool_call(
            "edit",
            Some("c2"),
            Some(&input),
            None,
            None,
            Some(150),
            1002,
        );

        let test_input = serde_json::json!({"command": "cargo test"});
        acc.append_tool_call(
            "bash",
            Some("c3"),
            Some(&test_input),
            Some("test result: ok"),
            None,
            Some(3200),
            1003,
        );

        let graph = acc.to_provenance_graph("agent", "Agent", "vendor", &[]);

        // Should serialize and deserialize via postcard (content-addressed format)
        let bytes = graph.serialize().unwrap();
        let (loaded, _hash) = pg::ProvenanceGraph::deserialize(&bytes).unwrap();

        assert_eq!(loaded.nodes.len(), 4);
        assert_eq!(loaded.edges.len(), graph.edges.len());
        assert_eq!(loaded.session_id, "sess-serialize");

        // Tool metadata should survive the round-trip
        let read_node = &loaded.nodes[1];
        assert_eq!(read_node.tool_name.as_deref(), Some("read"));

        let test_node = &loaded.nodes[3];
        assert_eq!(test_node.duration_ms, Some(3200));
    }

    #[test]
    fn test_to_provenance_graph_edge_kinds_convert() {
        let mut acc = ProvenanceAccumulator::new("sess-edges");
        let _goal = acc.append_goal("Fix", 1000);
        let _read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        let _edit = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

        let test_input = serde_json::json!({"command": "cargo test"});
        let _test = acc.append_tool_call(
            "bash",
            Some("c3"),
            Some(&test_input),
            None,
            None,
            None,
            1003,
        );

        let graph = acc.to_provenance_graph("a", "A", "v", &[]);

        // Verify edge kinds converted
        let edge_kinds: Vec<pg::ProvenanceEdgeKind> = graph.edges.iter().map(|e| e.kind).collect();

        assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::LedTo));
        assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::ExploredVia));
        assert!(edge_kinds.contains(&pg::ProvenanceEdgeKind::VerifiedBy));
    }

    // ---- Compaction summary ----

    #[test]
    fn test_compaction_summary_empty() {
        let acc = ProvenanceAccumulator::new("s1");
        let summary = acc.to_compaction_summary();
        assert!(summary.contains("0 nodes"));
    }

    #[test]
    fn test_compaction_summary_has_goals() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_goal("Fix the auth bug", 1000);

        let summary = acc.to_compaction_summary();
        assert!(summary.contains("### Goals"));
        assert!(summary.contains("Fix the auth bug"));
    }

    #[test]
    fn test_compaction_summary_has_changes() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_goal("Fix bug", 1000);

        let input = serde_json::json!({"path": "src/auth.rs"});
        acc.append_tool_call("edit", Some("c1"), Some(&input), None, None, None, 1001);

        let summary = acc.to_compaction_summary();
        assert!(summary.contains("### Changes Made"));
        assert!(summary.contains("Edit src/auth.rs"));
    }

    #[test]
    fn test_compaction_summary_has_patches() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_patch_proposal("ABCD1234", &["src/main.rs".into()], 1000);

        let summary = acc.to_compaction_summary();
        assert!(summary.contains("### Recorded Changes"));
    }

    #[test]
    fn test_compaction_summary_has_human_gates() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let gate = acc.append_human_gate("Should I delete old tokens?", 1000);

        let summary = acc.to_compaction_summary();
        assert!(summary.contains("### Human Gates"));
        assert!(summary.contains("pending"));

        acc.resolve_human_gate(&gate);
        let summary = acc.to_compaction_summary();
        assert!(summary.contains("resolved"));
    }

    #[test]
    fn test_compaction_summary_has_errors() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_tool_call(
            "edit",
            Some("c1"),
            None,
            Some("File not found"),
            Some("error"),
            None,
            1000,
        );

        let summary = acc.to_compaction_summary();
        assert!(summary.contains("### Errors"));
        assert!(summary.contains("failed"));
    }

    // ---- Persistence ----

    #[test]
    fn test_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("test-session");

        let mut acc = ProvenanceAccumulator::new("test-session");
        acc.append_goal("Fix bug", 1000);
        acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);

        acc.save(&session_dir).unwrap();

        // Verify file exists
        assert!(session_dir.join(GRAPH_FILENAME).exists());

        // Load it back
        let restored = ProvenanceAccumulator::load_or_create(&session_dir, "test-session").unwrap();
        assert_eq!(restored.node_count(), 3);
        assert_eq!(restored.edge_count(), acc.edge_count());
        assert_eq!(restored.session_id(), "test-session");
    }

    #[test]
    fn test_load_nonexistent_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let acc = ProvenanceAccumulator::load_or_create(dir.path(), "no-such-session").unwrap();
        assert!(acc.is_empty());
        assert_eq!(acc.session_id(), "no-such-session");
    }

    #[test]
    fn test_save_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep").join("nested").join("session");

        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_goal("Test", 1000);
        acc.save(&nested).unwrap();

        assert!(nested.join(GRAPH_FILENAME).exists());
    }

    #[test]
    fn test_incremental_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("s1");

        // First hook invocation: create session + goal
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_goal("Fix bug", 1000);
        acc.save(&session_dir).unwrap();

        // Second hook invocation: load + append tool call
        let mut acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
        assert_eq!(acc.node_count(), 1);
        acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
        acc.save(&session_dir).unwrap();

        // Third hook invocation: load + append another tool call
        let mut acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
        assert_eq!(acc.node_count(), 2);
        acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);
        acc.save(&session_dir).unwrap();

        // Final verification
        let acc = ProvenanceAccumulator::load_or_create(&session_dir, "s1").unwrap();
        assert_eq!(acc.node_count(), 3);
        assert_eq!(acc.stats().goal_count, 1);
        assert_eq!(acc.stats().exploration_count, 1);
        assert_eq!(acc.stats().commitment_count, 1);

        // Edges should be preserved through save/load cycles
        assert!(acc.edge_count() > 0);
    }

    // ---- Helper functions ----

    #[test]
    fn test_make_session_prefix_uuid() {
        let prefix = make_session_prefix("abc123de-f456-7890-abcd-ef1234567890");
        assert_eq!(prefix, "abc123de");
    }

    #[test]
    fn test_make_session_prefix_short() {
        let prefix = make_session_prefix("ab");
        assert_eq!(prefix, "ab");
    }

    #[test]
    fn test_make_session_prefix_empty() {
        let prefix = make_session_prefix("");
        assert_eq!(prefix, "s");
    }

    #[test]
    fn test_truncate_prompt_short() {
        assert_eq!(truncate_prompt("hello", 100), "hello");
    }

    #[test]
    fn test_truncate_prompt_long() {
        let long = "a ".repeat(300);
        let result = truncate_prompt(&long, 50);
        assert!(result.len() <= 50);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_prompt_trims() {
        assert_eq!(truncate_prompt("  hello  ", 100), "hello");
    }

    #[test]
    fn test_short_hash() {
        assert_eq!(short_hash("ABCDEF1234567890"), "ABCDEF12");
        assert_eq!(short_hash("SHORT"), "SHORT");
    }

    // ---- Node ID uniqueness ----

    #[test]
    fn test_node_ids_are_unique() {
        let mut acc = ProvenanceAccumulator::new("s1");
        let mut ids = Vec::new();

        ids.push(acc.append_goal("g1", 1000));
        ids.push(acc.append_tool_call("read", None, None, None, None, None, 1001));
        ids.push(acc.append_tool_call("edit", None, None, None, None, None, 1002));
        ids.push(acc.append_human_gate("gate", 1003));
        ids.push(acc.append_patch_proposal("HASH", &[], 1004));

        // All IDs should be unique
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "all node IDs should be unique");
    }

    #[test]
    fn test_node_ids_contain_session_prefix() {
        let mut acc = ProvenanceAccumulator::new("my-session-id");
        let id = acc.append_goal("test", 1000);
        assert!(
            id.starts_with("my"),
            "node ID should start with session prefix, got: {}",
            id
        );
    }

    // ---- Stats consistency ----

    #[test]
    fn test_stats_match_nodes() {
        let mut acc = ProvenanceAccumulator::new("s1");
        acc.append_goal("g", 1000);
        acc.append_tool_call("read", None, None, None, None, None, 1001);
        acc.append_tool_call("read", None, None, None, None, None, 1002);
        acc.append_tool_call("edit", None, None, None, None, None, 1003);

        let test_input = serde_json::json!({"command": "cargo test"});
        acc.append_tool_call("bash", None, Some(&test_input), None, None, None, 1004);

        let install_input = serde_json::json!({"command": "npm install"});
        acc.append_tool_call("bash", None, Some(&install_input), None, None, None, 1005);

        acc.append_tool_call("edit", None, None, None, Some("error"), None, 1006);
        acc.append_human_gate("proceed?", 1007);
        acc.append_patch_proposal("HASH", &[], 1008);

        let stats = acc.stats();
        assert_eq!(stats.goal_count, 1);
        assert_eq!(stats.exploration_count, 2);
        assert_eq!(stats.commitment_count, 1);
        assert_eq!(stats.verification_count, 1);
        assert_eq!(stats.execution_count, 1);
        assert_eq!(stats.error_count, 1);
        assert_eq!(stats.human_gate_count, 1);
        assert_eq!(stats.patch_proposal_count, 1);
        assert_eq!(stats.total_nodes(), acc.node_count() as u32);
        assert_eq!(stats.edge_count, acc.edge_count() as u32);
    }
}

//! Content-addressed provenance graph — causal decision DAG for agent sessions.
//!
//! A `ProvenanceGraph` is a content-addressed artifact that captures the causal
//! decision chain produced during an AI agent session. It sits alongside changes
//! and attestations in the Atomic graph:
//!
//! ```text
//! Content Graph:    A ──▶ B ──▶ C ──▶ D
//!
//! Dependency Graph: A ◀── Attest₁        (cost/tokens)
//!                   B ◀── Attest₁
//!                   A ◀── Provenance₁    (decision DAG)
//!                   B ◀── Provenance₁
//!                   C ◀── Provenance₂
//!                   D ◀── Provenance₂ ──▶ (previous: Provenance₁)
//! ```
//!
//! # Relationship to Attestations
//!
//! | Property | Attestation | ProvenanceGraph |
//! |----------|-------------|-----------------|
//! | Purpose | "How much did this session cost?" | "Why did the agent make this change?" |
//! | Content | Flat summary (cost, tokens, duration) | DAG (typed nodes + causal edges) |
//! | Magic | `ATST` | `PRVG` |
//! | Extension | `.attest` | `.provenance` |
//! | Node type | 2 | 3 |
//!
//! Both are content-addressed (Blake3), registered in EXTERNAL/INTERNAL and
//! NODE_TYPES, have dependencies on the changes they cover via the DEPS table,
//! and support session-resume chaining via a `previous` field.
//!
//! # File Format
//!
//! ```text
//! [MAGIC: 4 bytes "PRVG"]
//! [postcard payload → ProvenanceGraph]
//! ```
//!
//! # Split Storage (Privacy)
//!
//! The graph supports split storage for privacy:
//! - The **skeleton** (node kinds, summaries, edges, stats) is hashed and
//!   tamper-evident.
//! - The **detail** field on each node (raw tool args, outputs) can be stored
//!   in the change's unhashed section and stripped without invalidating the
//!   graph's content hash.
//!
//! This is implemented by serializing `detail` fields separately when
//! `strip_detail` is set on the builder.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::provenance_graph::{
//!     ProvenanceGraph, ProvenanceNode, ProvenanceEdge,
//!     ProvenanceNodeKind, ProvenanceEdgeKind, ProvenanceStats,
//! };
//! use atomic_core::types::Hash;
//!
//! let graph = ProvenanceGraph::builder("session-123", "claude-code")
//!     .agent_display_name("Claude Code")
//!     .agent_vendor("anthropic")
//!     .add_node(ProvenanceNode {
//!         id: "s-1".into(),
//!         kind: ProvenanceNodeKind::Goal,
//!         timestamp: 1735689600000,
//!         summary: "Fix the auth bug".into(),
//!         detail: None,
//!         change_hash: None,
//!         tool_name: None,
//!         tool_call_id: None,
//!         duration_ms: None,
//!         classified: false,
//!         confidence: None,
//!         consolidated_from: Vec::new(),
//!     })
//!     .add_node(ProvenanceNode {
//!         id: "s-2".into(),
//!         kind: ProvenanceNodeKind::Commitment,
//!         timestamp: 1735689601000,
//!         summary: "Edit src/auth.rs".into(),
//!         detail: None,
//!         change_hash: None,
//!         tool_name: Some("edit".into()),
//!         tool_call_id: Some("call-1".into()),
//!         duration_ms: Some(150),
//!         classified: false,
//!         confidence: None,
//!         consolidated_from: Vec::new(),
//!     })
//!     .add_edge(ProvenanceEdge {
//!         from: "s-1".into(),
//!         to: "s-2".into(),
//!         kind: ProvenanceEdgeKind::LedTo,
//!     })
//!     .add_change_explained(Hash::of(b"change-a"))
//!     .build();
//!
//! // Serialize
//! let bytes = graph.serialize().unwrap();
//! assert!(ProvenanceGraph::is_provenance_graph(&bytes));
//!
//! // Deserialize and verify hash
//! let (loaded, hash) = ProvenanceGraph::deserialize(&bytes).unwrap();
//! assert_eq!(loaded.session_id, "session-123");
//! assert_eq!(loaded.nodes.len(), 2);
//! assert_eq!(loaded.edges.len(), 1);
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

use crate::types::Hash;

// =============================================================================
// Constants
// =============================================================================

/// Magic bytes identifying a provenance graph file: "PRVG"
const MAGIC: &[u8; 4] = b"PRVG";

/// Current provenance graph schema version.
///
/// v1 → v2: added `profile: Option<String>` as the last field.
/// The `deserialize` method handles v1 payloads by deserializing into
/// `ProvenanceGraphV1` and upgrading to `ProvenanceGraph` in-memory.
const SCHEMA_VERSION: u8 = 2;

/// File extension for provenance graph files.
pub const PROVENANCE_GRAPH_EXTENSION: &str = "provenance";

/// Profile identifier for Sherpa-structured provenance graphs.
///
/// When `ProvenanceGraph.profile` is set to this value, the UI knows the
/// node `detail` fields contain Sherpa-specific structured JSON
/// (intent/todos/verification). Absent means a generic agent graph.
///
/// Semver rules:
/// - Minor bump (1.1.0): new optional fields added to any detail struct
/// - Major bump (2.0.0): breaking change to required fields
pub const SHERPA_PROFILE: &str = "sherpa-trace/1.0.0";

// =============================================================================
// ProvenanceGraph
// =============================================================================

/// A content-addressed provenance graph for an agent session segment.
///
/// Captures the causal decision chain that produced a set of changes.
/// Stored alongside change files, registered in the graph with
/// `node_type = PROVENANCE` (3).
///
/// Each turn recording produces one `ProvenanceGraph` artifact covering the
/// changes recorded in that turn. Multi-turn sessions chain graphs via
/// [`previous`](ProvenanceGraph::previous).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceGraph {
    /// Schema version for forward compatibility.
    pub version: u8,

    /// Timestamp when this graph was created (Unix epoch seconds).
    pub timestamp: i64,

    /// Session identifier linking provenance graphs within the same session.
    pub session_id: String,

    /// Agent registry key (e.g., "claude-code", "opencode").
    pub agent_name: String,

    /// Human-readable agent name (e.g., "Claude Code", "OpenCode").
    #[serde(default)]
    pub agent_display_name: String,

    /// AI vendor (e.g., "anthropic", "google", "openai").
    #[serde(default)]
    pub agent_vendor: String,

    /// The typed nodes in the decision DAG.
    pub nodes: Vec<ProvenanceNode>,

    /// Causal edges between nodes.
    pub edges: Vec<ProvenanceEdge>,

    /// Hashes of the changes this graph explains.
    ///
    /// Also registered in the DEPS table so the graph knows the
    /// relationship. Denormalized here for fast access.
    pub changes_explained: Vec<Hash>,

    /// Previous provenance graph in this session chain.
    ///
    /// On multi-turn sessions, each turn's graph chains to the previous
    /// one. Walk the chain to reconstruct the full session DAG.
    #[serde(default)]
    pub previous: Option<Hash>,

    /// Aggregate statistics for quick filtering without loading the full graph.
    pub stats: ProvenanceStats,

    /// Schema profile identifier.
    ///
    /// `None` — generic agent graph (atomic-agent, Claude Code, OpenCode).
    /// `Some("sherpa-trace/1.0.0")` — Sherpa structured graph: node `detail`
    /// fields carry intent/todo/execution/verification JSON. Use the
    /// [`SHERPA_PROFILE`] constant rather than a raw string literal.
    ///
    /// The UI checks this field first. If absent it falls back to generic
    /// rendering. If present it renders the full intent/todo/verification
    /// panels and applies the versioned detail schema.
    ///
    /// Must remain the LAST field in this struct. Postcard uses a positional
    /// binary format — appending here ensures old serialized graphs (which
    /// have no profile bytes) still deserialize correctly with `profile: None`.
    ///
    /// Note: do NOT add `skip_serializing_if` here. Postcard is a positional
    /// format — every field must always be present in the byte stream.
    /// Skipping would cause deserialization to read the wrong bytes for this
    /// field and corrupt the payload. `None` encodes as a single `0x00` byte.
    #[serde(default)]
    pub profile: Option<String>,
}

impl ProvenanceGraph {
    /// Create a builder for constructing a provenance graph.
    pub fn builder(
        session_id: impl Into<String>,
        agent_name: impl Into<String>,
    ) -> ProvenanceGraphBuilder {
        ProvenanceGraphBuilder::new(session_id, agent_name)
    }

    /// Serialize the provenance graph to bytes.
    ///
    /// Format: `[MAGIC: 4 bytes][postcard payload]`
    ///
    /// The hash is computed over the entire serialized output.
    pub fn serialize(&self) -> Result<Vec<u8>, ProvenanceGraphError> {
        let payload = postcard::to_allocvec(self).map_err(|e| ProvenanceGraphError::Codec {
            reason: format!("postcard serialize failed: {}", e),
        })?;

        let mut buf = Vec::with_capacity(MAGIC.len() + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Deserialize a provenance graph from bytes, returning the graph and its hash.
    ///
    /// The hash is computed over the input bytes (the entire serialized form).
    pub fn deserialize(data: &[u8]) -> Result<(Self, Hash), ProvenanceGraphError> {
        if data.len() < MAGIC.len() + 1 {
            return Err(ProvenanceGraphError::Codec {
                reason: format!(
                    "data too short: {} bytes (minimum {})",
                    data.len(),
                    MAGIC.len() + 1
                ),
            });
        }

        if &data[..4] != MAGIC {
            return Err(ProvenanceGraphError::Codec {
                reason: format!("invalid magic: expected {:?}, got {:?}", MAGIC, &data[..4]),
            });
        }

        // Peek at the version byte without fully deserializing.
        // The first field in the postcard payload is `version: u8`.
        let version_byte = data[4];

        if version_byte > SCHEMA_VERSION {
            return Err(ProvenanceGraphError::UnsupportedVersion {
                version: version_byte,
                max_supported: SCHEMA_VERSION,
            });
        }

        let graph: Self = if version_byte <= 1 {
            // v1 payload: no `profile` field. Deserialize into the v1 shim
            // struct (identical to the current struct minus `profile`) and
            // upgrade to v2 in-memory with `profile: None`.
            let v1: ProvenanceGraphV1 =
                postcard::from_bytes(&data[4..]).map_err(|e| ProvenanceGraphError::Codec {
                    reason: format!("postcard deserialize failed (v1): {}", e),
                })?;
            ProvenanceGraph {
                version: SCHEMA_VERSION,
                timestamp: v1.timestamp,
                session_id: v1.session_id,
                agent_name: v1.agent_name,
                agent_display_name: v1.agent_display_name,
                agent_vendor: v1.agent_vendor,
                nodes: v1.nodes,
                edges: v1.edges,
                changes_explained: v1.changes_explained,
                previous: v1.previous,
                stats: v1.stats,
                profile: None,
            }
        } else {
            postcard::from_bytes(&data[4..]).map_err(|e| ProvenanceGraphError::Codec {
                reason: format!("postcard deserialize failed: {}", e),
            })?
        };

        let hash = Hash::of(data);
        Ok((graph, hash))
    }

    /// Read a provenance graph from a reader (e.g., a file).
    pub fn read_from<R: Read>(reader: &mut R) -> Result<(Self, Hash), ProvenanceGraphError> {
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(ProvenanceGraphError::Io)?;
        Self::deserialize(&data)
    }

    /// Write a provenance graph to a writer (e.g., a file).
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<Hash, ProvenanceGraphError> {
        let data = self.serialize()?;
        let hash = Hash::of(&data);
        writer.write_all(&data).map_err(ProvenanceGraphError::Io)?;
        Ok(hash)
    }

    /// Check whether a byte slice looks like a provenance graph file.
    ///
    /// Fast check — only inspects the 4-byte magic prefix.
    pub fn is_provenance_graph(data: &[u8]) -> bool {
        data.len() >= MAGIC.len() && &data[..4] == MAGIC
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Number of changes explained by this graph.
    pub fn change_count(&self) -> usize {
        self.changes_explained.len()
    }

    /// Check if a specific change is explained by this graph.
    pub fn explains_change(&self, hash: &Hash) -> bool {
        self.changes_explained.contains(hash)
    }

    /// Check if this graph is part of a chain (has a predecessor).
    pub fn is_chained(&self) -> bool {
        self.previous.is_some()
    }

    /// Find a node by ID.
    pub fn find_node(&self, id: &str) -> Option<&ProvenanceNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Find all nodes of a given kind.
    pub fn nodes_of_kind(&self, kind: ProvenanceNodeKind) -> Vec<&ProvenanceNode> {
        self.nodes.iter().filter(|n| n.kind == kind).collect()
    }

    /// Find all edges originating from a node.
    pub fn edges_from(&self, node_id: &str) -> Vec<&ProvenanceEdge> {
        self.edges.iter().filter(|e| e.from == node_id).collect()
    }

    /// Find all edges targeting a node.
    pub fn edges_to(&self, node_id: &str) -> Vec<&ProvenanceEdge> {
        self.edges.iter().filter(|e| e.to == node_id).collect()
    }

    /// Walk backward from a node through its causal chain.
    ///
    /// Returns node IDs in reverse-causal order: the target node first,
    /// then its causes, then their causes, etc. Useful for answering
    /// "why did this change happen?"
    ///
    /// Stops at the graph boundary (goal nodes or nodes with no incoming edges).
    /// Avoids cycles (though the graph should be acyclic by construction).
    pub fn walk_backward(&self, start_node_id: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();

        queue.push_back(start_node_id.to_string());
        visited.insert(start_node_id.to_string());

        while let Some(node_id) = queue.pop_front() {
            result.push(node_id.clone());

            // Find all edges pointing TO this node (its causes)
            for edge in &self.edges {
                if edge.to == node_id && !visited.contains(&edge.from) {
                    visited.insert(edge.from.clone());
                    queue.push_back(edge.from.clone());
                }
            }
        }

        result
    }
}

impl fmt::Display for ProvenanceGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ProvenanceGraph — {} · {} nodes · {} edges · {} changes",
            self.agent_display_name,
            self.node_count(),
            self.edge_count(),
            self.change_count(),
        )?;

        let goals = self.nodes_of_kind(ProvenanceNodeKind::Goal);
        for goal in &goals {
            writeln!(f, "  Goal: {}", goal.summary)?;
        }

        writeln!(f, "  {}", self.stats)?;
        Ok(())
    }
}

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

// =============================================================================
// ProvenanceGraphV1  (migration shim — do not use for new code)
// =============================================================================

/// Schema v1 layout — identical to [`ProvenanceGraph`] but without `profile`.
///
/// Used only inside [`ProvenanceGraph::deserialize`] to read files written by
/// older versions of Atomic. After loading, the caller upgrades to the current
/// struct by setting `profile: None`.
#[derive(Serialize, Deserialize)]
struct ProvenanceGraphV1 {
    version: u8,
    timestamp: i64,
    session_id: String,
    agent_name: String,
    #[serde(default)]
    agent_display_name: String,
    #[serde(default)]
    agent_vendor: String,
    nodes: Vec<ProvenanceNode>,
    edges: Vec<ProvenanceEdge>,
    changes_explained: Vec<Hash>,
    #[serde(default)]
    previous: Option<Hash>,
    stats: ProvenanceStats,
}

// =============================================================================
// ProvenanceGraphBuilder
// =============================================================================

/// Builder for constructing `ProvenanceGraph` instances.
pub struct ProvenanceGraphBuilder {
    session_id: String,
    agent_name: String,
    agent_display_name: String,
    agent_vendor: String,
    profile: Option<String>,
    nodes: Vec<ProvenanceNode>,
    edges: Vec<ProvenanceEdge>,
    changes_explained: Vec<Hash>,
    previous: Option<Hash>,
    timestamp: Option<i64>,
}

impl ProvenanceGraphBuilder {
    /// Create a new builder with required fields.
    pub fn new(session_id: impl Into<String>, agent_name: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_name: agent_name.into(),
            agent_display_name: String::new(),
            agent_vendor: String::new(),
            profile: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            changes_explained: Vec::new(),
            previous: None,
            timestamp: None,
        }
    }

    /// Set the human-readable agent display name.
    pub fn agent_display_name(mut self, name: impl Into<String>) -> Self {
        self.agent_display_name = name.into();
        self
    }

    /// Set the AI vendor identifier.
    pub fn agent_vendor(mut self, vendor: impl Into<String>) -> Self {
        self.agent_vendor = vendor.into();
        self
    }

    /// Set the schema profile identifier.
    ///
    /// Use [`SHERPA_PROFILE`] for Sherpa-structured graphs. Omit for generic
    /// agent graphs — `profile` defaults to `None` if not set.
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Add a node to the graph.
    pub fn add_node(mut self, node: ProvenanceNode) -> Self {
        self.nodes.push(node);
        self
    }

    /// Set all nodes at once.
    pub fn nodes(mut self, nodes: Vec<ProvenanceNode>) -> Self {
        self.nodes = nodes;
        self
    }

    /// Add an edge to the graph.
    pub fn add_edge(mut self, edge: ProvenanceEdge) -> Self {
        self.edges.push(edge);
        self
    }

    /// Set all edges at once.
    pub fn edges(mut self, edges: Vec<ProvenanceEdge>) -> Self {
        self.edges = edges;
        self
    }

    /// Add a change hash that this graph explains.
    pub fn add_change_explained(mut self, hash: Hash) -> Self {
        self.changes_explained.push(hash);
        self
    }

    /// Set all explained change hashes at once.
    pub fn changes_explained(mut self, hashes: Vec<Hash>) -> Self {
        self.changes_explained = hashes;
        self
    }

    /// Set the previous provenance graph hash (for session resume chaining).
    pub fn previous(mut self, hash: Hash) -> Self {
        self.previous = Some(hash);
        self
    }

    /// Set the timestamp (Unix epoch seconds). Defaults to now.
    pub fn timestamp(mut self, ts: i64) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Build the provenance graph.
    pub fn build(self) -> ProvenanceGraph {
        let timestamp = self.timestamp.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

        let stats = ProvenanceStats::from_graph(&self.nodes, &self.edges);

        ProvenanceGraph {
            version: SCHEMA_VERSION,
            timestamp,
            session_id: self.session_id,
            agent_name: self.agent_name,
            agent_display_name: self.agent_display_name,
            agent_vendor: self.agent_vendor,
            nodes: self.nodes,
            edges: self.edges,
            changes_explained: self.changes_explained,
            previous: self.previous,
            stats,
            profile: self.profile,
        }
    }
}

// =============================================================================
// Error Type
// =============================================================================

/// Errors that can occur during provenance graph operations.
#[derive(Debug, thiserror::Error)]
pub enum ProvenanceGraphError {
    /// Serialization or deserialization failed.
    #[error("Provenance graph codec error: {reason}")]
    Codec {
        /// Description of what went wrong.
        reason: String,
    },

    /// The graph version is not supported.
    #[error("Unsupported provenance graph version: {version} (max supported: {max_supported})")]
    UnsupportedVersion {
        /// The version found in the data.
        version: u8,
        /// The maximum version this code supports.
        max_supported: u8,
    },

    /// I/O error reading or writing the graph.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Base32;

    // ---- Helpers ----

    fn make_goal(id: &str, summary: &str) -> ProvenanceNode {
        ProvenanceNode {
            id: id.into(),
            kind: ProvenanceNodeKind::Goal,
            timestamp: 1000,
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

    fn make_commitment(id: &str, summary: &str) -> ProvenanceNode {
        ProvenanceNode {
            id: id.into(),
            kind: ProvenanceNodeKind::Commitment,
            timestamp: 2000,
            summary: summary.into(),
            detail: None,
            change_hash: None,
            tool_name: Some("edit".into()),
            tool_call_id: Some("call-1".into()),
            duration_ms: Some(150),
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        }
    }

    fn make_exploration(id: &str, summary: &str) -> ProvenanceNode {
        ProvenanceNode {
            id: id.into(),
            kind: ProvenanceNodeKind::Exploration,
            timestamp: 1500,
            summary: summary.into(),
            detail: None,
            change_hash: None,
            tool_name: Some("read".into()),
            tool_call_id: None,
            duration_ms: None,
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        }
    }

    fn make_verification(id: &str, summary: &str) -> ProvenanceNode {
        ProvenanceNode {
            id: id.into(),
            kind: ProvenanceNodeKind::Verification,
            timestamp: 3000,
            summary: summary.into(),
            detail: None,
            change_hash: None,
            tool_name: Some("bash".into()),
            tool_call_id: None,
            duration_ms: Some(3200),
            classified: false,
            confidence: None,
            consolidated_from: Vec::new(),
        }
    }

    fn make_edge(from: &str, to: &str, kind: ProvenanceEdgeKind) -> ProvenanceEdge {
        ProvenanceEdge {
            from: from.into(),
            to: to.into(),
            kind,
        }
    }

    fn sample_graph() -> ProvenanceGraph {
        ProvenanceGraph::builder("sess-123", "claude-code")
            .agent_display_name("Claude Code")
            .agent_vendor("anthropic")
            .timestamp(1735689600)
            .add_node(make_goal("n-1", "Fix the auth bug"))
            .add_node(make_exploration("n-2", "Read src/auth.rs"))
            .add_node(make_exploration("n-3", "Read src/jwt.rs"))
            .add_node(make_commitment("n-4", "Edit src/auth.rs"))
            .add_node(make_verification("n-5", "cargo test --lib (passed)"))
            .add_edge(make_edge("n-1", "n-2", ProvenanceEdgeKind::LedTo))
            .add_edge(make_edge("n-1", "n-3", ProvenanceEdgeKind::LedTo))
            .add_edge(make_edge("n-2", "n-4", ProvenanceEdgeKind::ExploredVia))
            .add_edge(make_edge("n-3", "n-4", ProvenanceEdgeKind::ExploredVia))
            .add_edge(make_edge("n-4", "n-5", ProvenanceEdgeKind::VerifiedBy))
            .add_change_explained(Hash::of(b"change-a"))
            .build()
    }

    // ---- Builder ----

    #[test]
    fn test_builder_minimal() {
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .build();

        assert_eq!(graph.version, 2);
        assert_eq!(graph.timestamp, 1000);
        assert_eq!(graph.session_id, "sess-1");
        assert_eq!(graph.agent_name, "agent");
        assert!(graph.agent_display_name.is_empty());
        assert!(graph.agent_vendor.is_empty());
        assert!(graph.profile.is_none());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.changes_explained.is_empty());
        assert!(graph.previous.is_none());
        assert!(graph.stats.is_empty());
    }

    #[test]
    fn test_builder_with_sherpa_profile() {
        let graph = ProvenanceGraph::builder("sess-1", "sherpa")
            .timestamp(1000)
            .profile(SHERPA_PROFILE)
            .build();

        assert_eq!(graph.profile, Some(SHERPA_PROFILE.to_string()));
    }

    #[test]
    fn test_profile_none_by_default() {
        let graph = sample_graph();
        assert!(graph.profile.is_none());
    }

    #[test]
    fn test_profile_roundtrips_through_serialization() {
        let graph = ProvenanceGraph::builder("sess-1", "sherpa")
            .timestamp(1000)
            .profile(SHERPA_PROFILE)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert_eq!(loaded.profile, Some(SHERPA_PROFILE.to_string()));
    }

    #[test]
    fn test_profile_absent_on_old_graph_deserializes_as_none() {
        // Simulate a v1 payload: build a ProvenanceGraphV1 directly and
        // serialize it with postcard (no profile field), then wrap it with
        // the PRVG magic and verify that deserialize upgrades it to v2 with
        // profile = None.
        let v1 = ProvenanceGraphV1 {
            version: 1,
            timestamp: 500,
            session_id: "sess-old".into(),
            agent_name: "claude-code".into(),
            agent_display_name: String::new(),
            agent_vendor: String::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            changes_explained: Vec::new(),
            previous: None,
            stats: ProvenanceStats::default(),
        };

        let payload = postcard::to_allocvec(&v1).unwrap();
        let mut bytes = b"PRVG".to_vec();
        bytes.extend_from_slice(&payload);

        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert!(loaded.profile.is_none());
        // Version is upgraded to current schema version in memory.
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.session_id, "sess-old");
    }

    #[test]
    fn test_builder_full() {
        let graph = sample_graph();

        assert_eq!(graph.session_id, "sess-123");
        assert_eq!(graph.agent_name, "claude-code");
        assert_eq!(graph.agent_display_name, "Claude Code");
        assert_eq!(graph.agent_vendor, "anthropic");
        assert_eq!(graph.nodes.len(), 5);
        assert_eq!(graph.edges.len(), 5);
        assert_eq!(graph.changes_explained.len(), 1);
        assert!(graph.previous.is_none());
    }

    #[test]
    fn test_builder_auto_computes_stats() {
        let graph = sample_graph();

        assert_eq!(graph.stats.goal_count, 1);
        assert_eq!(graph.stats.exploration_count, 2);
        assert_eq!(graph.stats.commitment_count, 1);
        assert_eq!(graph.stats.verification_count, 1);
        assert_eq!(graph.stats.edge_count, 5);
        assert_eq!(graph.stats.total_nodes(), 5);
    }

    #[test]
    fn test_builder_with_previous() {
        let prev_hash = Hash::of(b"previous-graph");
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .previous(prev_hash)
            .build();

        assert!(graph.is_chained());
        assert_eq!(graph.previous, Some(prev_hash));
    }

    #[test]
    fn test_builder_with_changes_explained() {
        let h1 = Hash::of(b"change-1");
        let h2 = Hash::of(b"change-2");
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .changes_explained(vec![h1, h2])
            .build();

        assert_eq!(graph.change_count(), 2);
        assert!(graph.explains_change(&h1));
        assert!(graph.explains_change(&h2));
        assert!(!graph.explains_change(&Hash::of(b"other")));
    }

    #[test]
    fn test_builder_timestamp_defaults_to_now() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let graph = ProvenanceGraph::builder("sess-1", "agent").build();

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert!(graph.timestamp >= before);
        assert!(graph.timestamp <= after);
    }

    // ---- Serialization ----

    #[test]
    fn test_serialize_has_magic() {
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .build();
        let bytes = graph.serialize().unwrap();

        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"PRVG");
    }

    #[test]
    fn test_is_provenance_graph() {
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .build();
        let bytes = graph.serialize().unwrap();

        assert!(ProvenanceGraph::is_provenance_graph(&bytes));
        assert!(!ProvenanceGraph::is_provenance_graph(b"ATST"));
        assert!(!ProvenanceGraph::is_provenance_graph(b"PRV"));
        assert!(!ProvenanceGraph::is_provenance_graph(b""));
        assert!(!ProvenanceGraph::is_provenance_graph(b"hello world"));
    }

    #[test]
    fn test_serialize_deserialize_roundtrip_minimal() {
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, hash) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.session_id, "sess-1");
        assert_eq!(loaded.agent_name, "agent");
        assert_eq!(loaded.timestamp, 1000);
        assert!(!hash.to_base32().is_empty());
    }

    #[test]
    fn test_serialize_deserialize_roundtrip_full() {
        let graph = sample_graph();

        let bytes = graph.serialize().unwrap();
        let (loaded, _hash) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert_eq!(loaded.session_id, "sess-123");
        assert_eq!(loaded.agent_name, "claude-code");
        assert_eq!(loaded.agent_display_name, "Claude Code");
        assert_eq!(loaded.agent_vendor, "anthropic");
        assert_eq!(loaded.nodes.len(), 5);
        assert_eq!(loaded.edges.len(), 5);
        assert_eq!(loaded.changes_explained.len(), 1);
        assert_eq!(loaded.stats.goal_count, 1);
        assert_eq!(loaded.stats.exploration_count, 2);
        assert_eq!(loaded.stats.commitment_count, 1);
        assert_eq!(loaded.stats.verification_count, 1);
        assert_eq!(loaded.stats.edge_count, 5);
    }

    #[test]
    fn test_serialize_deserialize_with_chaining() {
        let prev_hash = Hash::of(b"prev");
        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .previous(prev_hash)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert!(loaded.is_chained());
        assert_eq!(loaded.previous, Some(prev_hash));
    }

    #[test]
    fn test_serialize_deterministic() {
        let graph = sample_graph();

        let bytes1 = graph.serialize().unwrap();
        let bytes2 = graph.serialize().unwrap();

        assert_eq!(bytes1, bytes2);
        assert_eq!(Hash::of(&bytes1), Hash::of(&bytes2));
    }

    #[test]
    fn test_deserialize_too_short() {
        let result = ProvenanceGraph::deserialize(b"PRV");
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_wrong_magic() {
        let result = ProvenanceGraph::deserialize(b"ATSTsomedata");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid magic"));
    }

    #[test]
    fn test_deserialize_corrupt_payload() {
        let mut data = Vec::new();
        data.extend_from_slice(b"PRVG");
        data.extend_from_slice(b"this is not valid postcard data");

        let result = ProvenanceGraph::deserialize(&data);
        assert!(result.is_err());
    }

    // ---- Read/Write ----

    #[test]
    fn test_write_read_roundtrip() {
        let graph = sample_graph();

        let mut buf = Vec::new();
        let write_hash = graph.write_to(&mut buf).unwrap();

        let (loaded, read_hash) = ProvenanceGraph::read_from(&mut buf.as_slice()).unwrap();

        assert_eq!(write_hash, read_hash);
        assert_eq!(loaded.session_id, "sess-123");
        assert_eq!(loaded.nodes.len(), 5);
    }

    // ---- Node queries ----

    #[test]
    fn test_find_node() {
        let graph = sample_graph();

        let node = graph.find_node("n-1").unwrap();
        assert_eq!(node.kind, ProvenanceNodeKind::Goal);
        assert_eq!(node.summary, "Fix the auth bug");

        assert!(graph.find_node("nonexistent").is_none());
    }

    #[test]
    fn test_nodes_of_kind() {
        let graph = sample_graph();

        let goals = graph.nodes_of_kind(ProvenanceNodeKind::Goal);
        assert_eq!(goals.len(), 1);

        let explorations = graph.nodes_of_kind(ProvenanceNodeKind::Exploration);
        assert_eq!(explorations.len(), 2);

        let decisions = graph.nodes_of_kind(ProvenanceNodeKind::Decision);
        assert!(decisions.is_empty());
    }

    // ---- Edge queries ----

    #[test]
    fn test_edges_from() {
        let graph = sample_graph();

        let from_goal = graph.edges_from("n-1");
        assert_eq!(from_goal.len(), 2);
        assert!(from_goal
            .iter()
            .all(|e| e.kind == ProvenanceEdgeKind::LedTo));

        let from_commit = graph.edges_from("n-4");
        assert_eq!(from_commit.len(), 1);
        assert_eq!(from_commit[0].kind, ProvenanceEdgeKind::VerifiedBy);
    }

    #[test]
    fn test_edges_to() {
        let graph = sample_graph();

        let to_commit = graph.edges_to("n-4");
        assert_eq!(to_commit.len(), 2);
        assert!(to_commit
            .iter()
            .all(|e| e.kind == ProvenanceEdgeKind::ExploredVia));
    }

    // ---- Backward traversal ----

    #[test]
    fn test_walk_backward_from_verification() {
        let graph = sample_graph();

        // Walk backward from the verification node
        let chain = graph.walk_backward("n-5");

        // Should include: n-5 (verification) ← n-4 (commitment) ← n-2, n-3 (explorations) ← n-1 (goal)
        assert!(chain.contains(&"n-5".to_string()));
        assert!(chain.contains(&"n-4".to_string()));
        assert!(chain.contains(&"n-2".to_string()));
        assert!(chain.contains(&"n-3".to_string()));
        assert!(chain.contains(&"n-1".to_string()));
        assert_eq!(chain.len(), 5);

        // First element should be the start node
        assert_eq!(chain[0], "n-5");
    }

    #[test]
    fn test_walk_backward_from_goal() {
        let graph = sample_graph();

        // Walking backward from the goal — it's a root, so only itself
        let chain = graph.walk_backward("n-1");
        assert_eq!(chain, vec!["n-1"]);
    }

    #[test]
    fn test_walk_backward_nonexistent() {
        let graph = sample_graph();

        let chain = graph.walk_backward("nonexistent");
        assert_eq!(chain, vec!["nonexistent"]);
    }

    // ---- Node types ----

    #[test]
    fn test_node_kind_serde_roundtrip() {
        let kinds = [
            ProvenanceNodeKind::Goal,
            ProvenanceNodeKind::Exploration,
            ProvenanceNodeKind::Decision,
            ProvenanceNodeKind::Commitment,
            ProvenanceNodeKind::Verification,
            ProvenanceNodeKind::Execution,
            ProvenanceNodeKind::HumanGate,
            ProvenanceNodeKind::PatchProposal,
            ProvenanceNodeKind::Error,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: ProvenanceNodeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    #[test]
    fn test_node_kind_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProvenanceNodeKind::HumanGate).unwrap(),
            "\"human_gate\""
        );
        assert_eq!(
            serde_json::to_string(&ProvenanceNodeKind::PatchProposal).unwrap(),
            "\"patch_proposal\""
        );
    }

    #[test]
    fn test_edge_kind_serde_roundtrip() {
        let kinds = [
            ProvenanceEdgeKind::LedTo,
            ProvenanceEdgeKind::ExploredVia,
            ProvenanceEdgeKind::CommittedVia,
            ProvenanceEdgeKind::VerifiedBy,
            ProvenanceEdgeKind::BlockedBy,
            ProvenanceEdgeKind::ResumedAfter,
            ProvenanceEdgeKind::FailedWith,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: ProvenanceEdgeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    // ---- ProvenanceStats ----

    #[test]
    fn test_stats_from_graph() {
        let nodes = vec![
            make_goal("n-1", "Goal"),
            make_exploration("n-2", "Read"),
            make_commitment("n-3", "Edit"),
        ];
        let edges = vec![
            make_edge("n-1", "n-2", ProvenanceEdgeKind::LedTo),
            make_edge("n-2", "n-3", ProvenanceEdgeKind::ExploredVia),
        ];

        let stats = ProvenanceStats::from_graph(&nodes, &edges);

        assert_eq!(stats.goal_count, 1);
        assert_eq!(stats.exploration_count, 1);
        assert_eq!(stats.commitment_count, 1);
        assert_eq!(stats.edge_count, 2);
        assert_eq!(stats.total_nodes(), 3);
        assert!(!stats.is_empty());
    }

    #[test]
    fn test_stats_default_is_empty() {
        let stats = ProvenanceStats::default();
        assert!(stats.is_empty());
        assert_eq!(stats.total_nodes(), 0);
    }

    #[test]
    fn test_stats_display() {
        let mut stats = ProvenanceStats::default();
        stats.goal_count = 1;
        stats.commitment_count = 2;
        stats.edge_count = 3;

        let display = format!("{}", stats);
        assert!(display.contains("1 goal"));
        assert!(display.contains("2 commitments"));
        assert!(display.contains("3 edges"));
    }

    #[test]
    fn test_stats_display_empty() {
        let stats = ProvenanceStats::default();
        assert_eq!(format!("{}", stats), "empty graph");
    }

    // ---- Display ----

    #[test]
    fn test_graph_display() {
        let graph = sample_graph();
        let display = format!("{}", graph);

        assert!(display.contains("Claude Code"));
        assert!(display.contains("5 nodes"));
        assert!(display.contains("5 edges"));
        assert!(display.contains("Fix the auth bug"));
    }

    #[test]
    fn test_node_display() {
        let node = make_goal("n-1", "Fix the bug");
        let display = format!("{}", node);

        assert!(display.contains("n-1"));
        assert!(display.contains("goal"));
        assert!(display.contains("Fix the bug"));
    }

    #[test]
    fn test_edge_display() {
        let edge = make_edge("a", "b", ProvenanceEdgeKind::LedTo);
        let display = format!("{}", edge);

        assert!(display.contains("a"));
        assert!(display.contains("b"));
        assert!(display.contains("led_to"));
    }

    // ---- Node detail ----

    #[test]
    fn test_node_with_detail() {
        let mut node = make_commitment("n-1", "Edit file");
        node.detail = Some(r#"{"file":"src/auth.rs","tool":"edit"}"#.into());

        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .add_node(node)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        let loaded_node = &loaded.nodes[0];
        assert!(loaded_node.detail.is_some());
        assert!(loaded_node.detail.as_ref().unwrap().contains("src/auth.rs"));
    }

    #[test]
    fn test_node_with_change_hash() {
        let hash = Hash::of(b"my-change");
        let mut node = make_commitment("n-1", "Edit file");
        node.change_hash = Some(hash);

        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .add_node(node)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        assert_eq!(loaded.nodes[0].change_hash, Some(hash));
    }

    #[test]
    fn test_node_classified_fields() {
        let mut node = make_goal("n-1", "Explored auth → chose JWT fix");
        node.kind = ProvenanceNodeKind::Decision;
        node.classified = true;
        node.confidence = Some(0.92);
        node.consolidated_from = vec!["n-2".into(), "n-3".into(), "n-4".into()];

        let graph = ProvenanceGraph::builder("sess-1", "agent")
            .timestamp(1000)
            .add_node(node)
            .build();

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();

        let loaded_node = &loaded.nodes[0];
        assert!(loaded_node.classified);
        assert!((loaded_node.confidence.unwrap() - 0.92).abs() < 0.001);
        assert_eq!(loaded_node.consolidated_from, vec!["n-2", "n-3", "n-4"]);
    }

    // ---- Error type ----

    #[test]
    fn test_error_display_codec() {
        let err = ProvenanceGraphError::Codec {
            reason: "bad data".into(),
        };
        assert!(err.to_string().contains("bad data"));
    }

    #[test]
    fn test_error_display_version() {
        let err = ProvenanceGraphError::UnsupportedVersion {
            version: 99,
            max_supported: 1,
        };
        let display = err.to_string();
        assert!(display.contains("99"));
        assert!(display.contains("1"));
    }

    // ---- Size / performance ----

    #[test]
    fn test_serialized_size_reasonable() {
        let graph = sample_graph();
        let bytes = graph.serialize().unwrap();

        // A 5-node graph should serialize to a few hundred bytes, not kilobytes
        assert!(bytes.len() < 2000, "serialized size: {} bytes", bytes.len());
        assert!(
            bytes.len() > MAGIC.len(),
            "serialized size should exceed magic"
        );
    }

    #[test]
    fn test_large_graph_serializes() {
        let mut builder = ProvenanceGraph::builder("sess-large", "agent").timestamp(1000);

        // Build a 100-node graph
        for i in 0..100 {
            builder = builder.add_node(ProvenanceNode {
                id: format!("n-{}", i),
                kind: if i == 0 {
                    ProvenanceNodeKind::Goal
                } else if i % 3 == 0 {
                    ProvenanceNodeKind::Exploration
                } else if i % 3 == 1 {
                    ProvenanceNodeKind::Commitment
                } else {
                    ProvenanceNodeKind::Verification
                },
                timestamp: 1000 + i as i64,
                summary: format!("Node {}", i),
                detail: None,
                change_hash: None,
                tool_name: Some("tool".into()),
                tool_call_id: None,
                duration_ms: None,
                classified: false,
                confidence: None,
                consolidated_from: Vec::new(),
            });

            if i > 0 {
                builder = builder.add_edge(ProvenanceEdge {
                    from: format!("n-{}", i - 1),
                    to: format!("n-{}", i),
                    kind: ProvenanceEdgeKind::LedTo,
                });
            }
        }

        let graph = builder.build();
        assert_eq!(graph.node_count(), 100);
        assert_eq!(graph.edge_count(), 99);

        let bytes = graph.serialize().unwrap();
        let (loaded, _) = ProvenanceGraph::deserialize(&bytes).unwrap();
        assert_eq!(loaded.node_count(), 100);

        // 100 nodes should still be well under 50KB
        assert!(
            bytes.len() < 50_000,
            "100-node graph serialized to {} bytes",
            bytes.len()
        );
    }

    // ---- Cross-compatibility with serde_json ----

    #[test]
    fn test_node_kind_compatible_with_agent_types() {
        // Verify that the JSON representation matches the agent-side types.
        // The agent uses serde_json for graph.json; this crate uses postcard
        // for .provenance files. But the enum variants must match for any
        // JSON-based interchange.
        let kinds = [
            ("goal", ProvenanceNodeKind::Goal),
            ("exploration", ProvenanceNodeKind::Exploration),
            ("decision", ProvenanceNodeKind::Decision),
            ("commitment", ProvenanceNodeKind::Commitment),
            ("verification", ProvenanceNodeKind::Verification),
            ("execution", ProvenanceNodeKind::Execution),
            ("human_gate", ProvenanceNodeKind::HumanGate),
            ("patch_proposal", ProvenanceNodeKind::PatchProposal),
            ("error", ProvenanceNodeKind::Error),
        ];

        for (expected_json, kind) in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", expected_json),
                "NodeKind::{:?} JSON mismatch",
                kind
            );
        }
    }

    #[test]
    fn test_edge_kind_compatible_with_agent_types() {
        let kinds = [
            ("led_to", ProvenanceEdgeKind::LedTo),
            ("explored_via", ProvenanceEdgeKind::ExploredVia),
            ("committed_via", ProvenanceEdgeKind::CommittedVia),
            ("verified_by", ProvenanceEdgeKind::VerifiedBy),
            ("blocked_by", ProvenanceEdgeKind::BlockedBy),
            ("resumed_after", ProvenanceEdgeKind::ResumedAfter),
            ("failed_with", ProvenanceEdgeKind::FailedWith),
        ];

        for (expected_json, kind) in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", expected_json),
                "EdgeKind::{:?} JSON mismatch",
                kind
            );
        }
    }
}

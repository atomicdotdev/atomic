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

mod builder;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types
pub use builder::{ProvenanceGraphBuilder, ProvenanceGraphError};
pub use types::{
    ProvenanceEdge, ProvenanceEdgeKind, ProvenanceNode, ProvenanceNodeKind, ProvenanceStats,
};

// Re-export the V1 shim for internal use only
pub(crate) use builder::{ProvenanceGraphV1, ProvenanceGraphV2};

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Read, Write};

use crate::types::Hash;

// =============================================================================
// Constants
// =============================================================================

/// Magic bytes identifying a provenance graph file: "PRVG"
pub(crate) const MAGIC: &[u8; 4] = b"PRVG";

/// Current provenance graph schema version.
///
/// v1 → v2: added `profile: Option<String>` as the last field.
/// The `deserialize` method handles v1 payloads by deserializing into
/// `ProvenanceGraphV1` and upgrading to `ProvenanceGraph` in-memory.
pub(crate) const SCHEMA_VERSION: u8 = 3;

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
    /// Postcard uses a positional format: compatibility fields are appended
    /// after existing fields and use `serde(default)`. Never reorder fields or
    /// use `skip_serializing_if` here.
    #[serde(default)]
    pub profile: Option<String>,

    /// Stable vault work-item/intent ID governing this turn, when known.
    #[serde(default)]
    pub plan_id: Option<String>,

    /// Generic todo snapshot captured at turn end.
    #[serde(default)]
    pub todos: Vec<crate::change::session::SessionTodo>,
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
                plan_id: None,
                todos: Vec::new(),
            }
        } else if version_byte == 2 {
            let v2: ProvenanceGraphV2 =
                postcard::from_bytes(&data[4..]).map_err(|e| ProvenanceGraphError::Codec {
                    reason: format!("postcard deserialize failed (v2): {}", e),
                })?;
            ProvenanceGraph {
                version: SCHEMA_VERSION,
                timestamp: v2.timestamp,
                session_id: v2.session_id,
                agent_name: v2.agent_name,
                agent_display_name: v2.agent_display_name,
                agent_vendor: v2.agent_vendor,
                nodes: v2.nodes,
                edges: v2.edges,
                changes_explained: v2.changes_explained,
                previous: v2.previous,
                stats: v2.stats,
                profile: v2.profile,
                plan_id: None,
                todos: Vec::new(),
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

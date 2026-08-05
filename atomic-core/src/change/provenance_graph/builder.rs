//! Builder, migration shim, and error type for provenance graphs.

use serde::{Deserialize, Serialize};
use std::io;

use crate::types::Hash;

use super::types::{ProvenanceEdge, ProvenanceNode, ProvenanceStats};
use super::{ProvenanceGraph, SCHEMA_VERSION};

// =============================================================================
// ProvenanceGraphV1  (migration shim — do not use for new code)
// =============================================================================

/// Schema v1 layout — identical to [`ProvenanceGraph`] but without `profile`.
///
/// Used only inside [`ProvenanceGraph::deserialize`] to read files written by
/// older versions of Atomic. After loading, the caller upgrades to the current
/// struct by setting `profile: None`.
#[derive(Serialize, Deserialize)]
pub(crate) struct ProvenanceGraphV1 {
    pub version: u8,
    pub timestamp: i64,
    pub session_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub agent_display_name: String,
    #[serde(default)]
    pub agent_vendor: String,
    pub nodes: Vec<ProvenanceNode>,
    pub edges: Vec<ProvenanceEdge>,
    pub changes_explained: Vec<Hash>,
    #[serde(default)]
    pub previous: Option<Hash>,
    pub stats: ProvenanceStats,
}

/// Schema v2 layout — adds `profile`, but predates generic plan/todo context.
#[derive(Serialize, Deserialize)]
pub(crate) struct ProvenanceGraphV2 {
    pub version: u8,
    pub timestamp: i64,
    pub session_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub agent_display_name: String,
    #[serde(default)]
    pub agent_vendor: String,
    pub nodes: Vec<ProvenanceNode>,
    pub edges: Vec<ProvenanceEdge>,
    pub changes_explained: Vec<Hash>,
    #[serde(default)]
    pub previous: Option<Hash>,
    pub stats: ProvenanceStats,
    #[serde(default)]
    pub profile: Option<String>,
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
    plan_id: Option<String>,
    todos: Vec<crate::change::session::SessionTodo>,
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
            plan_id: None,
            todos: Vec::new(),
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
    /// Use [`super::SHERPA_PROFILE`] for Sherpa-structured graphs. Omit for generic
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

    /// Set the vault work-item/intent governing this turn.
    pub fn plan_id(mut self, plan_id: impl Into<String>) -> Self {
        self.plan_id = Some(plan_id.into());
        self
    }

    /// Set the generic todo snapshot captured at turn end.
    pub fn todos(mut self, todos: Vec<crate::change::session::SessionTodo>) -> Self {
        self.todos = todos;
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
            plan_id: self.plan_id,
            todos: self.todos,
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

use std::path::Path;

use atomic_core::change::provenance_graph as pg;
use atomic_core::types::{Base32, Hash};

use super::helpers::{convert_edge_kind, convert_node_kind};
use super::{PreparedProvenanceGraph, ProvenanceAccumulator, GRAPH_FILENAME};
use crate::error::{AgentError, AgentResult};
use crate::provenance::types::{GraphNode, NodeKind, SerializedGraph};

impl ProvenanceAccumulator {
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
        super::super::consolidate::consolidate(
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
        let prepared = self.prepare_provenance_graph(
            agent_name,
            agent_display_name,
            agent_vendor,
            changes_explained,
        );
        self.nodes_saved_count = prepared.nodes_end;
        self.edges_saved_count = prepared.edges_end;
        prepared.graph
    }

    /// Build the next per-turn graph without advancing durable accumulator cursors.
    pub fn prepare_provenance_graph(
        &self,
        agent_name: &str,
        agent_display_name: &str,
        agent_vendor: &str,
        changes_explained: &[Hash],
    ) -> PreparedProvenanceGraph {
        let previous = self
            .last_provenance_hash
            .as_ref()
            .and_then(|s| Hash::from_base32(s.as_bytes()));
        let new_nodes = &self.nodes[self.nodes_saved_count..];
        let new_edges = &self.edges[self.edges_saved_count..];
        let new_node_ids: std::collections::HashSet<&str> =
            new_nodes.iter().map(|node| node.id.as_str()).collect();
        let relevant_edges: Vec<&super::super::types::GraphEdge> = new_edges
            .iter()
            .filter(|edge| {
                new_node_ids.contains(edge.from.as_str()) || new_node_ids.contains(edge.to.as_str())
            })
            .collect();
        let nodes = new_nodes
            .iter()
            .map(|node| pg::ProvenanceNode {
                id: node.id.clone(),
                kind: convert_node_kind(node.kind),
                timestamp: node.timestamp,
                summary: node.summary.clone(),
                detail: node.detail.as_ref().map(ToString::to_string),
                change_hash: node
                    .change_hash
                    .as_ref()
                    .and_then(|hash| Hash::from_base32(hash.as_bytes())),
                tool_name: node.tool_name.clone(),
                tool_call_id: node.tool_call_id.clone(),
                duration_ms: node.duration_ms,
                classified: node.classified,
                confidence: node.confidence,
                consolidated_from: node.consolidated_from.clone(),
            })
            .collect();
        let edges = relevant_edges
            .iter()
            .map(|edge| pg::ProvenanceEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
                kind: convert_edge_kind(edge.kind),
            })
            .collect();
        let timestamp = new_nodes
            .iter()
            .map(|node| node.timestamp)
            .max()
            .unwrap_or(0);
        let mut builder = pg::ProvenanceGraph::builder(&self.session_id, agent_name)
            .agent_display_name(agent_display_name)
            .agent_vendor(agent_vendor)
            .nodes(nodes)
            .edges(edges)
            .changes_explained(changes_explained.to_vec())
            .timestamp(timestamp);
        if let Some(previous) = previous {
            builder = builder.previous(previous);
        }
        PreparedProvenanceGraph {
            graph: builder.build(),
            nodes_end: self.nodes.len(),
            edges_end: self.edges.len(),
        }
    }

    /// Advance cursors only after the prepared graph and session head publish.
    pub fn acknowledge_prepared_graph(&mut self, prepared: &PreparedProvenanceGraph, hash: Hash) {
        self.nodes_saved_count = self.nodes_saved_count.max(prepared.nodes_end);
        self.edges_saved_count = self.edges_saved_count.max(prepared.edges_end);
        self.last_provenance_hash = Some(hash.to_base32());
    }

    /// Advance all current cursors after an externally prepared graph publishes.
    pub fn acknowledge_published_graph(&mut self, hash: Hash) {
        self.nodes_saved_count = self.nodes.len();
        self.edges_saved_count = self.edges.len();
        self.last_provenance_hash = Some(hash.to_base32());
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

    /// Write the legacy graph encoding for migration tooling and fixtures.
    ///
    /// Production hook dispatch never calls this after the redb cutover.
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
}

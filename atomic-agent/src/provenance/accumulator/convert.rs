use std::path::Path;

use atomic_core::change::provenance_graph as pg;
use atomic_core::types::{Base32, Hash};

use super::helpers::{convert_edge_kind, convert_node_kind};
use super::{ProvenanceAccumulator, GRAPH_FILENAME};
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
        let relevant_edges: Vec<&super::super::types::GraphEdge> = new_edges
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
}

//! Knowledge graph trait for node/edge CRUD and queries.

use crate::pristine::error::PristineError;
use crate::pristine::vault::{KgEdge, KgNode, KgSubgraph};

/// Read-only knowledge graph operations.
pub trait KgTxnT {
    /// Get a KG node by its ID.
    fn get_kg_node(&self, id: &str) -> Result<Option<KgNode>, PristineError>;

    /// Get all outgoing edges from a node.
    fn get_kg_edges_from(&self, node_id: &str) -> Result<Vec<KgEdge>, PristineError>;

    /// Get all incoming edges to a node.
    fn get_kg_edges_to(&self, node_id: &str) -> Result<Vec<KgEdge>, PristineError>;

    /// Get the 1-hop neighborhood of a node (outgoing + incoming edges + connected nodes).
    fn kg_neighbors(&self, node_id: &str, depth: u8) -> Result<KgSubgraph, PristineError> {
        let depth = depth.min(2);
        let mut visited = std::collections::HashSet::new();
        let mut result_edges = Vec::new();
        let mut frontier = vec![node_id.to_string()];
        visited.insert(node_id.to_string());

        for _ in 0..depth {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier = Vec::new();

            for current_id in &frontier {
                for edge in self.get_kg_edges_from(current_id)? {
                    if visited.insert(edge.to_id.clone()) {
                        next_frontier.push(edge.to_id.clone());
                    }
                    result_edges.push(edge);
                }
                for edge in self.get_kg_edges_to(current_id)? {
                    if visited.insert(edge.from_id.clone()) {
                        next_frontier.push(edge.from_id.clone());
                    }
                    result_edges.push(edge);
                }
            }

            frontier = next_frontier;
        }

        // Deduplicate edges
        {
            let mut seen = std::collections::HashSet::new();
            result_edges
                .retain(|e| seen.insert((e.from_id.clone(), e.to_id.clone(), e.kind.clone())));
        }

        // Collect nodes
        let mut nodes = Vec::new();
        for id in &visited {
            if let Some(node) = self.get_kg_node(id)? {
                nodes.push(node);
            }
        }

        Ok(KgSubgraph {
            nodes,
            edges: result_edges,
        })
    }

    /// Full-text search over node IDs, labels, and summaries.
    fn kg_fts_search(&self, query: &str, limit: usize) -> Result<Vec<KgNode>, PristineError>;

    /// Return ALL matching node IDs with hit counts (no truncation, no node fetching).
    ///
    /// This is the low-level primitive for callers that need to rank results
    /// themselves before fetching full nodes.  Returns `(node_id, hit_count)`
    /// pairs for every node that matches at least one query token.
    fn kg_fts_match_ids(&self, query: &str) -> Result<Vec<(String, usize)>, PristineError>;

    /// Count total nodes in the KG.
    fn count_kg_nodes(&self) -> Result<usize, PristineError>;

    /// Return the IDs of all KG nodes whose `source` field matches one of
    /// `sources`.
    ///
    /// Used to select derived nodes (e.g. `"vcs"` and `"ast"` for the
    /// VCS/tree-sitter enrichment pipeline) so a rebuild can drop them before
    /// re-deriving, without touching vault- or session-sourced nodes.
    fn kg_node_ids_by_source(&self, sources: &[&str]) -> Result<Vec<String>, PristineError>;

    /// Count total edges in the KG.
    fn count_kg_edges(&self) -> Result<usize, PristineError>;
}

/// Write operations on the knowledge graph.
pub trait KgMutTxnT: KgTxnT {
    /// Insert or update a KG node, replacing its previous full-text tokens.
    fn upsert_kg_node(&mut self, node: &KgNode) -> Result<(), PristineError>;

    /// Insert a KG edge (idempotent — ignores duplicates).
    fn upsert_kg_edge(&mut self, edge: &KgEdge) -> Result<(), PristineError>;

    /// Delete a KG node, all its edges, and all its full-text tokens.
    fn del_kg_node(&mut self, id: &str) -> Result<bool, PristineError>;

    /// Delete a specific edge.
    fn del_kg_edge(
        &mut self,
        from_id: &str,
        to_id: &str,
        kind: &str,
    ) -> Result<bool, PristineError>;

    /// Delete all edges from/to a node (but keep the node).
    fn del_kg_edges_for_node(&mut self, node_id: &str) -> Result<usize, PristineError>;

    /// Initialize the KG tables.
    fn init_kg(&mut self) -> Result<(), PristineError>;
}

//! Alive graph traversal and content ordering
//!
//! This module handles traversing the repository graph to determine which
//! vertices are "alive" (not deleted) and in what order they should appear
//! in the output file.
//!
//! # Overview
//!
//! When outputting a file, we need to:
//!
//! 1. **Retrieve** the subgraph for that file (starting from its inode position)
//! 2. **Classify** vertices as alive, deleted, or zombie
//! 3. **Order** vertices to produce linear output (handling conflicts)
//! 4. **Output** content in the determined order
//!
//! # Graph Structure
//!
//! The alive graph is a directed graph where:
//!
//! - **Vertices** represent chunks of content (from changes)
//! - **Edges** represent ordering relationships between chunks
//! - **Flags** indicate span state (zombie, visited, etc.)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Alive Graph Example                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │      ┌───────┐                                                          │
//! │      │ ROOT  │ (empty inode span)                                     │
//! │      └───┬───┘                                                          │
//! │          │                                                              │
//! │          ▼                                                              │
//! │      ┌───────┐                                                          │
//! │      │ V1    │ "fn main() {\n"                                          │
//! │      └───┬───┘                                                          │
//! │          │                                                              │
//! │     ┌────┴────┐   <- Order conflict: two changes added here             │
//! │     ▼         ▼                                                         │
//! │ ┌───────┐ ┌───────┐                                                     │
//! │ │ V2a   │ │ V2b   │  "    // Comment A\n" vs "    // Comment B\n"       │
//! │ └───┬───┘ └───┬───┘                                                     │
//! │     └────┬────┘                                                         │
//! │          ▼                                                              │
//! │      ┌───────┐                                                          │
//! │      │ V3    │ "}\n"                                                    │
//! │      └───────┘                                                          │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Strongly Connected Components (SCCs)
//!
//! The graph may contain cycles (from conflicting changes). We use Tarjan's
//! algorithm to find SCCs, which are then output as cyclic conflicts.
//!
//! # Module Structure
//!
//! - [`graph`]: The `AliveGraph` data structure and accessors
//! - [`span`]: `AliveVertex` and span classification
//! - [`retrieve`]: Graph retrieval from the pristine database
//! - [`order`]: Topological ordering with conflict detection
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::alive::{AliveGraph, retrieve_graph};
//!
//! // Retrieve the graph for a file
//! let graph = retrieve_graph(&txn, &channel, file_position, false)?;
//!
//! // Compute ordering (with SCCs for conflicts)
//! let sccs = graph.tarjan();
//! let (conflict_tree, forward_edges) = graph.dfs(&sccs);
//!
//! // Output in order
//! output_graph(&changes, &txn, &channel, &mut writer, &graph, &sccs, conflict_tree)?;
//! ```

mod graph;
mod order;
mod retrieve;
mod vertex;

pub use graph::{AliveGraph, GraphStats};
pub use order::{compute_order, ConflictPath, ConflictTree, OrderResult, PathElement, SccId};
pub use retrieve::{retrieve_graph, RetrieveOptions, RetrieveResult};
pub use vertex::{AliveVertex, VertexFlags, VertexId};

use crate::types::{GraphNode, NodeId, SerializedGraphEdge};

/// A redundant forward edge that should be cleaned up.
///
/// During output, we may discover forward edges that skip over content.
/// These are artifacts of how changes were applied and can be removed
/// to simplify the graph.
#[derive(Debug, Clone)]
pub struct RedundantEdge {
    /// The source node of the redundant edge.
    pub node: GraphNode<NodeId>,
    /// The edge to remove.
    pub edge: SerializedGraphEdge,
}

impl RedundantEdge {
    /// Create a new redundant edge record.
    pub fn new(node: GraphNode<NodeId>, edge: SerializedGraphEdge) -> Self {
        Self { node, edge }
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangePosition, EdgeFlags, Position};

    #[test]
    fn test_redundant_edge_new() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let edge = SerializedGraphEdge::new(
            EdgeFlags::BLOCK,
            Position::new(NodeId::new(2), ChangePosition::new(0)),
            NodeId::new(1),
        );

        let redundant = RedundantEdge::new(node, edge);
        assert_eq!(redundant.node.change, NodeId::new(1));
    }

    #[test]
    fn test_redundant_edge_debug() {
        let node = GraphNode::ROOT;
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, Position::ROOT, NodeId::ROOT);

        let redundant = RedundantEdge::new(node, edge);
        let debug = format!("{:?}", redundant);
        assert!(debug.contains("RedundantEdge"));
    }

    #[test]
    fn test_redundant_edge_clone() {
        let node = GraphNode::new(
            NodeId::new(5),
            ChangePosition::new(10),
            ChangePosition::new(20),
        );
        let edge = SerializedGraphEdge::new(
            EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
            Position::new(NodeId::new(6), ChangePosition::new(5)),
            NodeId::new(5),
        );

        let original = RedundantEdge::new(node, edge);
        let cloned = original.clone();

        assert_eq!(original.node, cloned.node);
    }
}

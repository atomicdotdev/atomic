//! Fork conflict detection for the alive graph.
//!
//! A **fork conflict** occurs when a parent vertex has multiple children
//! that landed in *different* SCCs — meaning there is no ordering between
//! them.  This is the most common conflict type when two agents edit the
//! same location concurrently: both add content after the same parent
//! vertex, but neither depends on the other.
//!
//! Unlike cyclic conflicts (where Tarjan finds a cycle), fork children
//! live in separate single-vertex SCCs.  The standard `scc.len() > 1`
//! check never fires for them, so they must be detected separately.
//!
//! # Algorithm
//!
//! 1. Build a `VertexId → SCC index` map from the order result.
//! 2. For each non-dummy vertex with ≥2 children, check whether the
//!    children span multiple SCCs.
//! 3. Filter out structural vertices (empty inodes, root sentinels) —
//!    only non-empty content vertices are relevant.
//! 4. Return the list of [`ForkConflict`]s for the merge engine.

use std::collections::HashMap;

use crate::output::alive::{AliveGraph, OrderResult, VertexId};

/// A detected fork conflict in the alive graph.
///
/// Represents a parent vertex whose children have no ordering between
/// them (they reside in different SCCs).
pub(crate) struct ForkConflict {
    /// The parent vertex that has multiple unordered children.
    pub parent: VertexId,
    /// The child vertices that form the fork (≥2, all non-empty content).
    pub children: Vec<VertexId>,
}

/// Detect fork conflicts in the alive graph.
///
/// A fork is a vertex with ≥2 children that reside in different SCCs
/// (i.e. there is no ordering between them).  Only non-empty, non-root
/// content children are considered — structural markers (empty inode
/// vertices) are filtered out.
///
/// # Arguments
///
/// * `graph` - The alive graph to scan for forks
/// * `order` - The computed SCC ordering from Tarjan's algorithm
///
/// # Returns
///
/// A (possibly empty) list of fork conflicts.
pub(crate) fn detect_fork_conflicts(graph: &AliveGraph, order: &OrderResult) -> Vec<ForkConflict> {
    let mut forks = Vec::new();

    // Build vertex → SCC-index map
    let mut vertex_to_scc: HashMap<VertexId, usize> = HashMap::new();
    for (scc_idx, scc) in order.sccs.iter().enumerate() {
        for &vid in scc {
            vertex_to_scc.insert(vid, scc_idx);
        }
    }

    let vertex_count = graph.len_vertices();
    for vid_raw in 0..vertex_count {
        let vid = VertexId::new(vid_raw);
        if vid.is_dummy() {
            continue;
        }

        if graph.child_count(vid) <= 1 {
            continue;
        }

        // Collect non-dummy children
        let children: Vec<VertexId> = graph
            .children(vid)
            .map(|(_, child_vid)| *child_vid)
            .filter(|c: &VertexId| !c.is_dummy())
            .collect();

        if children.len() <= 1 {
            continue;
        }

        // Are the children spread across different SCCs?
        let first_scc = vertex_to_scc.get(&children[0]);
        let all_same_scc = children.iter().all(|c| vertex_to_scc.get(c) == first_scc);
        if all_same_scc {
            continue; // Already ordered or in a cyclic SCC — not a fork
        }

        // Keep only non-empty content vertices (skip inodes / structural markers)
        let content_children: Vec<VertexId> = children
            .into_iter()
            .filter(|&c| {
                graph
                    .try_get_vertex(c)
                    .map(|v| !v.node.is_root() && !v.node.is_empty())
                    .unwrap_or(false)
            })
            .collect();

        if content_children.len() > 1 {
            forks.push(ForkConflict {
                parent: vid,
                children: content_children,
            });
        }
    }

    forks
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::alive::{AliveGraph, AliveVertex, OrderResult};
    use crate::types::{
        ChangePosition, EdgeFlags, GraphNode, NodeId, Position, SerializedGraphEdge,
    };

    fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(
            NodeId::new(change),
            ChangePosition::new(start),
            ChangePosition::new(end),
        )
    }

    /// Build a graph where the vertex at `parent_idx` has the given child
    /// vertex indices.  Vertices are pushed in order 0..N where 0 is DUMMY.
    fn build_graph(
        vertices: &[GraphNode<NodeId>],
        parent_idx: usize,
        child_indices: &[usize],
    ) -> AliveGraph {
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, Position::ROOT, NodeId::ROOT);

        let mut graph = AliveGraph::new();

        for (i, v) in vertices.iter().enumerate() {
            if i == 0 {
                graph.push_vertex(AliveVertex::DUMMY);
            } else {
                graph.push_vertex(AliveVertex::new(*v));
            }

            if i == parent_idx {
                graph.set_last_children_start();
                for &ci in child_indices {
                    graph.push_child_to_last(Some(edge), VertexId::new(ci));
                }
            }
        }
        graph
    }

    #[test]
    fn no_fork_single_child() {
        let verts = [
            GraphNode::BOTTOM,
            make_vertex(1, 0, 10),
            make_vertex(2, 0, 10),
        ];
        let graph = build_graph(&verts, 1, &[2]);

        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        assert!(detect_fork_conflicts(&graph, &order).is_empty());
    }

    #[test]
    fn fork_two_children_different_sccs() {
        let verts = [
            GraphNode::BOTTOM,
            make_vertex(1, 0, 10),
            make_vertex(2, 0, 10),
            make_vertex(3, 0, 10),
        ];
        let graph = build_graph(&verts, 1, &[2, 3]);

        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)], vec![VertexId(3)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let forks = detect_fork_conflicts(&graph, &order);
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0].parent, VertexId::new(1));
        assert_eq!(forks[0].children.len(), 2);
        assert!(forks[0].children.contains(&VertexId::new(2)));
        assert!(forks[0].children.contains(&VertexId::new(3)));
    }

    #[test]
    fn skips_empty_inode_children() {
        let verts = [
            GraphNode::BOTTOM,
            make_vertex(1, 0, 10), // parent
            make_vertex(2, 5, 5),  // empty (inode)
            make_vertex(3, 0, 10), // content
        ];
        let graph = build_graph(&verts, 1, &[2, 3]);

        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)], vec![VertexId(3)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        assert!(detect_fork_conflicts(&graph, &order).is_empty());
    }

    #[test]
    fn children_same_scc_no_fork() {
        let verts = [
            GraphNode::BOTTOM,
            make_vertex(1, 0, 10),
            make_vertex(2, 0, 10),
            make_vertex(3, 0, 10),
        ];
        let graph = build_graph(&verts, 1, &[2, 3]);

        // Both children in the same SCC (cyclic conflict, not a fork)
        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2), VertexId(3)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 1,
            forward_edges: Vec::new(),
        };

        assert!(detect_fork_conflicts(&graph, &order).is_empty());
    }

    #[test]
    fn three_way_fork() {
        let verts = [
            GraphNode::BOTTOM,
            make_vertex(1, 0, 10),
            make_vertex(2, 0, 5),
            make_vertex(3, 0, 7),
            make_vertex(4, 0, 3),
        ];
        let graph = build_graph(&verts, 1, &[2, 3, 4]);

        let order = OrderResult {
            sccs: vec![
                vec![VertexId(1)],
                vec![VertexId(2)],
                vec![VertexId(3)],
                vec![VertexId(4)],
            ],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let forks = detect_fork_conflicts(&graph, &order);
        assert_eq!(forks.len(), 1);
        assert_eq!(forks[0].children.len(), 3);
    }

    #[test]
    fn no_children_at_all() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(make_vertex(1, 0, 10)));

        let order = OrderResult {
            sccs: vec![vec![VertexId(1)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        assert!(detect_fork_conflicts(&graph, &order).is_empty());
    }

    #[test]
    fn empty_graph() {
        let graph = AliveGraph::new();
        let order = OrderResult {
            sccs: Vec::new(),
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        assert!(detect_fork_conflicts(&graph, &order).is_empty());
    }
}

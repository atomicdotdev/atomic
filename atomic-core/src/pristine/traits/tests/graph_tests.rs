use crate::pristine::error::PristineError;
use crate::pristine::traits::graph::GraphTxnT;
use crate::pristine::traits::vertex_ext::VertexExt;
use crate::types::{
    ChangePosition, Edge, EdgeFlags, EdgeKind, GraphNode, Hash, NodeId, ParentEdgeKind, Position,
    SerializedGraphEdge,
};

// ── MockGraph for testing default iter_forward / iter_parents ──

/// Minimal mock `GraphTxnT` that stores a flat list of (vertex, edge) pairs.
struct MockGraph {
    edges: Vec<(GraphNode<NodeId>, SerializedGraphEdge)>,
}

impl MockGraph {
    fn new() -> Self {
        Self { edges: Vec::new() }
    }

    fn add(
        &mut self,
        node: GraphNode<NodeId>,
        flags: EdgeFlags,
        dest_change: u64,
        dest_pos: u64,
        introduced_by: u64,
    ) {
        let dest = Position {
            change: NodeId::new(dest_change),
            pos: ChangePosition::new(dest_pos),
        };
        let edge = SerializedGraphEdge::new(flags, dest, NodeId::new(introduced_by));
        self.edges.push((node, edge));
    }
}

impl GraphTxnT for MockGraph {
    type Adj = std::vec::IntoIter<Result<SerializedGraphEdge, PristineError>>;

    fn get_external(&self, _id: NodeId) -> Result<Option<Hash>, PristineError> {
        Ok(None)
    }

    fn get_internal(&self, _hash: &Hash) -> Result<Option<NodeId>, PristineError> {
        Ok(None)
    }

    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError> {
        let edges: Vec<Result<SerializedGraphEdge, PristineError>> = self
            .edges
            .iter()
            .filter(|(n, _)| *n == node)
            .map(|(_, e)| *e)
            .filter(|e| {
                let flag = e.flag();
                flag >= min_flag && flag <= max_flag
            })
            .map(Ok)
            .collect();
        Ok(edges.into_iter())
    }

    fn find_block(&self, _pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        Err(PristineError::BlockNotFound { change: 0, pos: 0 })
    }

    fn find_block_end(&self, _pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        Err(PristineError::BlockNotFound { change: 0, pos: 0 })
    }

    fn has_vertex(&self, _node: GraphNode<NodeId>) -> Result<bool, PristineError> {
        Ok(false)
    }

    fn get_node_type(&self, _node_id: NodeId) -> Result<Option<u8>, PristineError> {
        Ok(None)
    }

    fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        Ok(Vec::new())
    }

    fn has_change_in_graph(&self, _change_id: NodeId) -> Result<bool, PristineError> {
        Ok(false)
    }
}

/// Helper: build a `MockGraph` with one vertex that has every valid edge kind.
fn mock_with_all_edges() -> (MockGraph, GraphNode<NodeId>) {
    let node = GraphNode::from_parts(NodeId::new(1), 0, 10);
    let mut g = MockGraph::new();

    // Forward alive
    g.add(node, EdgeFlags::BLOCK, 2, 0, 10);
    g.add(node, EdgeFlags::FOLDER, 2, 100, 10);
    g.add(node, EdgeFlags::PSEUDO | EdgeFlags::BLOCK, 2, 200, 10);
    g.add(node, EdgeFlags::PSEUDO | EdgeFlags::FOLDER, 2, 300, 10);
    // Forward deleted
    g.add(node, EdgeFlags::BLOCK | EdgeFlags::DELETED, 2, 400, 10);
    g.add(node, EdgeFlags::FOLDER | EdgeFlags::DELETED, 2, 500, 10);

    // Parent alive
    g.add(node, EdgeFlags::PARENT | EdgeFlags::BLOCK, 3, 0, 10);
    g.add(node, EdgeFlags::PARENT | EdgeFlags::FOLDER, 3, 100, 10);
    g.add(
        node,
        EdgeFlags::PARENT | EdgeFlags::PSEUDO | EdgeFlags::BLOCK,
        3,
        200,
        10,
    );
    g.add(
        node,
        EdgeFlags::PARENT | EdgeFlags::PSEUDO | EdgeFlags::FOLDER,
        3,
        300,
        10,
    );
    // Parent deleted
    g.add(
        node,
        EdgeFlags::PARENT | EdgeFlags::BLOCK | EdgeFlags::DELETED,
        3,
        400,
        10,
    );
    g.add(
        node,
        EdgeFlags::PARENT | EdgeFlags::FOLDER | EdgeFlags::DELETED,
        3,
        500,
        10,
    );

    (g, node)
}

// ── iter_forward tests ──────────────────────────────────────

#[test]
fn iter_forward_alive_only() {
    let (g, node) = mock_with_all_edges();
    let fwd = g.iter_forward(node, false).unwrap();
    assert_eq!(fwd.len(), 4);
    assert!(fwd.iter().all(|e| !e.kind.is_deleted()));
    let kinds: Vec<_> = fwd.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&EdgeKind::Block));
    assert!(kinds.contains(&EdgeKind::Folder));
    assert!(kinds.contains(&EdgeKind::PseudoBlock));
    assert!(kinds.contains(&EdgeKind::PseudoFolder));
}

#[test]
fn iter_forward_with_deleted() {
    let (g, node) = mock_with_all_edges();
    let fwd = g.iter_forward(node, true).unwrap();
    assert_eq!(fwd.len(), 6);
    let kinds: Vec<_> = fwd.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&EdgeKind::Block));
    assert!(kinds.contains(&EdgeKind::Folder));
    assert!(kinds.contains(&EdgeKind::PseudoBlock));
    assert!(kinds.contains(&EdgeKind::PseudoFolder));
    assert!(kinds.contains(&EdgeKind::BlockDeleted));
    assert!(kinds.contains(&EdgeKind::FolderDeleted));
}

#[test]
fn iter_forward_never_returns_parent_edges() {
    let (g, node) = mock_with_all_edges();
    // Even with include_deleted (wider range) no parent edges leak through
    for edge in g.iter_forward(node, true).unwrap() {
        let flags = edge.kind.to_flags();
        assert!(
            !flags.contains(EdgeFlags::PARENT),
            "iter_forward returned a parent edge: {:?}",
            edge
        );
    }
}

// ── iter_parents tests ──────────────────────────────────────

#[test]
fn iter_parents_alive_only() {
    let (g, node) = mock_with_all_edges();
    let parents = g.iter_parents(node, false).unwrap();
    assert_eq!(parents.len(), 4);
    assert!(parents.iter().all(|e| !e.kind.is_deleted()));
    let kinds: Vec<_> = parents.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&ParentEdgeKind::Block));
    assert!(kinds.contains(&ParentEdgeKind::Folder));
    assert!(kinds.contains(&ParentEdgeKind::PseudoBlock));
    assert!(kinds.contains(&ParentEdgeKind::PseudoFolder));
}

#[test]
fn iter_parents_with_deleted() {
    let (g, node) = mock_with_all_edges();
    let parents = g.iter_parents(node, true).unwrap();
    assert_eq!(parents.len(), 6);
    let kinds: Vec<_> = parents.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&ParentEdgeKind::Block));
    assert!(kinds.contains(&ParentEdgeKind::Folder));
    assert!(kinds.contains(&ParentEdgeKind::PseudoBlock));
    assert!(kinds.contains(&ParentEdgeKind::PseudoFolder));
    assert!(kinds.contains(&ParentEdgeKind::BlockDeleted));
    assert!(kinds.contains(&ParentEdgeKind::FolderDeleted));
}

#[test]
fn iter_parents_never_returns_forward_edges() {
    let (g, node) = mock_with_all_edges();
    for edge in g.iter_parents(node, true).unwrap() {
        let flags = edge.kind.to_flags();
        assert!(
            flags.contains(EdgeFlags::PARENT),
            "iter_parents returned a forward edge: {:?}",
            edge
        );
    }
}

// ── consistency: forward + parent == total valid edges ───────

#[test]
fn typed_iteration_covers_all_valid_edges() {
    let (g, node) = mock_with_all_edges();

    let forward_count = g.iter_forward(node, true).unwrap().len();
    let parent_count = g.iter_parents(node, true).unwrap().len();

    // Count how many raw edges parse to a valid Edge variant
    let all_raw = g
        .iter_adjacent(node, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap();
    let valid_count = all_raw
        .filter_map(|r| r.ok())
        .filter_map(|e| Edge::from_serialized(&e))
        .count();

    assert_eq!(
        forward_count + parent_count,
        valid_count,
        "iter_forward({}) + iter_parents({}) != valid Edge count ({})",
        forward_count,
        parent_count,
        valid_count,
    );
}

// ── edge payload is preserved ───────────────────────────────

#[test]
fn iter_forward_preserves_dest_and_introduced_by() {
    let node = GraphNode::from_parts(NodeId::new(1), 0, 10);
    let mut g = MockGraph::new();
    g.add(node, EdgeFlags::BLOCK, 42, 99, 7);

    let fwd = g.iter_forward(node, false).unwrap();
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0].dest.change, NodeId::new(42));
    assert_eq!(fwd[0].dest.pos.get(), 99);
    assert_eq!(fwd[0].introduced_by, NodeId::new(7));
}

#[test]
fn iter_parents_preserves_dest_and_introduced_by() {
    let node = GraphNode::from_parts(NodeId::new(1), 0, 10);
    let mut g = MockGraph::new();
    g.add(node, EdgeFlags::PARENT | EdgeFlags::FOLDER, 55, 77, 3);

    let parents = g.iter_parents(node, false).unwrap();
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].dest.change, NodeId::new(55));
    assert_eq!(parents[0].dest.pos.get(), 77);
    assert_eq!(parents[0].introduced_by, NodeId::new(3));
    assert_eq!(parents[0].kind, ParentEdgeKind::Folder);
}

// ── empty vertex returns empty results ──────────────────────

#[test]
fn iter_forward_on_vertex_with_no_edges() {
    let node = GraphNode::from_parts(NodeId::new(99), 0, 5);
    let g = MockGraph::new();
    assert!(g.iter_forward(node, true).unwrap().is_empty());
}

#[test]
fn iter_parents_on_vertex_with_no_edges() {
    let node = GraphNode::from_parts(NodeId::new(99), 0, 5);
    let g = MockGraph::new();
    assert!(g.iter_parents(node, true).unwrap().is_empty());
}

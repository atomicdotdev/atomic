use super::super::graph::AliveGraph;
use super::super::vertex::{AliveVertex, VertexFlags, VertexId};
use super::options::{RetrieveOptions, RetrieveResult};
use super::retrieve_graph;
use crate::pristine::{GraphTxnT, GraphVisibilityClosure, PristineError};
use crate::types::{
    ChangePosition, EdgeFlags, EdgeKind, ForwardEdge, GraphNode, Hash, NodeId, Position,
    SerializedGraphEdge,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn visibility(changes: impl IntoIterator<Item = NodeId>) -> GraphVisibilityClosure {
    GraphVisibilityClosure::from_ordered_unchecked(changes)
}

#[derive(Clone, Copy)]
enum Fault {
    Inconsistent(&'static str),
    BlockNotFound { change: u64, pos: u64 },
}

impl Fault {
    fn into_error(self) -> PristineError {
        match self {
            Self::Inconsistent(message) => PristineError::Inconsistent {
                message: message.to_string(),
            },
            Self::BlockNotFound { change, pos } => PristineError::BlockNotFound { change, pos },
        }
    }
}

#[derive(Default)]
struct FaultGraphTxn {
    edges: HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>>,
    blocks: HashMap<Position<NodeId>, GraphNode<NodeId>>,
    block_ends: HashMap<Position<NodeId>, GraphNode<NodeId>>,
    find_block_faults: HashMap<Position<NodeId>, Fault>,
    find_block_end_faults: HashMap<Position<NodeId>, Fault>,
    forward_item_faults: HashMap<GraphNode<NodeId>, Fault>,
    parent_item_faults: HashMap<GraphNode<NodeId>, Fault>,
}

impl FaultGraphTxn {
    fn add_edge(&mut self, source: GraphNode<NodeId>, flags: EdgeFlags, dest: Position<NodeId>) {
        self.edges
            .entry(source)
            .or_default()
            .push(SerializedGraphEdge::new(flags, dest, NodeId::new(99)));
    }

    fn resolve_block(&mut self, pos: Position<NodeId>, node: GraphNode<NodeId>) {
        self.blocks.insert(pos, node);
    }

    fn resolve_block_end(&mut self, pos: Position<NodeId>, node: GraphNode<NodeId>) {
        self.block_ends.insert(pos, node);
    }

    fn fail_find_block(&mut self, pos: Position<NodeId>, fault: Fault) {
        self.find_block_faults.insert(pos, fault);
    }

    fn fail_find_block_end(&mut self, pos: Position<NodeId>, fault: Fault) {
        self.find_block_end_faults.insert(pos, fault);
    }

    fn fail_forward_item(&mut self, node: GraphNode<NodeId>, fault: Fault) {
        self.forward_item_faults.insert(node, fault);
    }

    fn fail_parent_item(&mut self, node: GraphNode<NodeId>, fault: Fault) {
        self.parent_item_faults.insert(node, fault);
    }
}

impl GraphTxnT for FaultGraphTxn {
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
        let item_fault = if min_flag.contains(EdgeFlags::PARENT) {
            self.parent_item_faults.get(&node)
        } else {
            self.forward_item_faults.get(&node)
        };
        if let Some(fault) = item_fault {
            return Ok(vec![Err(fault.into_error())].into_iter());
        }

        let edges = self
            .edges
            .get(&node)
            .into_iter()
            .flatten()
            .filter(|edge| {
                let flag = edge.flag();
                flag >= min_flag && flag <= max_flag
            })
            .copied()
            .map(Ok)
            .collect::<Vec<_>>();
        Ok(edges.into_iter())
    }

    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        if let Some(fault) = self.find_block_faults.get(&pos) {
            return Err(fault.into_error());
        }
        self.blocks
            .get(&pos)
            .copied()
            .ok_or(PristineError::BlockNotFound {
                change: pos.change.get(),
                pos: pos.pos.get(),
            })
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        if let Some(fault) = self.find_block_end_faults.get(&pos) {
            return Err(fault.into_error());
        }
        self.block_ends
            .get(&pos)
            .copied()
            .ok_or(PristineError::BlockNotFound {
                change: pos.change.get(),
                pos: pos.pos.get(),
            })
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError> {
        Ok(self.edges.contains_key(&node))
    }

    fn get_node_type(&self, _node_id: NodeId) -> Result<Option<u8>, PristineError> {
        Ok(None)
    }

    fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        Ok(Vec::new())
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
        Ok(self.edges.keys().any(|node| node.change == change_id))
    }
}

fn test_pos(change: u64, pos: u64) -> Position<NodeId> {
    Position::new(NodeId::new(change), ChangePosition::new(pos))
}

fn test_node(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
    GraphNode::new(
        NodeId::new(change),
        ChangePosition::new(start),
        ChangePosition::new(end),
    )
}

fn assert_inconsistent(
    result: Result<RetrieveResult, PristineError>,
    expected_message: &'static str,
) {
    match result {
        Err(PristineError::Inconsistent { message }) => {
            assert_eq!(message, expected_message);
        }
        Err(other) => panic!("expected Inconsistent({expected_message:?}), got {other:?}"),
        Ok(_) => panic!("expected Inconsistent({expected_message:?}), got success"),
    }
}

fn add_dead_walk_prefix(
    txn: &mut FaultGraphTxn,
) -> (Position<NodeId>, GraphNode<NodeId>, Position<NodeId>) {
    let start = test_pos(1, 0);
    let root = start.inode_node();
    let dead_pos = test_pos(2, 0);
    let dead = test_node(2, 0, 10);
    let dead_end = test_pos(2, 10);

    txn.add_edge(root, EdgeFlags::BLOCK, dead_pos);
    txn.resolve_block(dead_pos, dead);

    (start, dead, dead_end)
}

fn add_visible_chain_fixture(
    txn: &mut FaultGraphTxn,
) -> (Position<NodeId>, GraphNode<NodeId>, GraphNode<NodeId>) {
    let (start, dead, dead_end) = add_dead_walk_prefix(txn);
    let first_pos = test_pos(3, 0);
    let first = test_node(3, 0, 10);
    let second_pos = test_pos(4, 0);
    let second = test_node(4, 0, 10);

    txn.add_edge(dead, EdgeFlags::BLOCK, first_pos);
    txn.add_edge(dead, EdgeFlags::BLOCK, second_pos);
    txn.resolve_block(first_pos, first);
    txn.resolve_block(second_pos, second);
    txn.add_edge(first, EdgeFlags::PARENT | EdgeFlags::BLOCK, dead_end);
    txn.add_edge(second, EdgeFlags::PARENT | EdgeFlags::BLOCK, dead_end);
    txn.resolve_block_end(dead_end, dead);

    (start, first, second)
}

// -------------------------------------------------------------------------
// Fail-closed resolver propagation tests
// -------------------------------------------------------------------------

#[test]
fn adjacency_item_error_propagates() {
    let start = test_pos(1, 0);
    let mut txn = FaultGraphTxn::default();
    txn.fail_forward_item(
        start.inode_node(),
        Fault::Inconsistent("forward adjacency item"),
    );

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "forward adjacency item",
    );
}

#[test]
fn direct_destination_resolution_error_propagates() {
    let start = test_pos(1, 0);
    let dest = test_pos(7, 42);
    let mut txn = FaultGraphTxn::default();
    txn.add_edge(start.inode_node(), EdgeFlags::BLOCK, dest);
    txn.fail_find_block(dest, Fault::BlockNotFound { change: 7, pos: 42 });

    match retrieve_graph(&txn, start, RetrieveOptions::new()) {
        Err(PristineError::BlockNotFound { change, pos }) => {
            assert_eq!((change, pos), (7, 42));
        }
        Err(other) => panic!("expected BlockNotFound(7, 42), got {other:?}"),
        Ok(_) => panic!("expected BlockNotFound(7, 42), got success"),
    }
}

#[test]
fn dead_walk_destination_resolution_error_propagates() {
    let mut txn = FaultGraphTxn::default();
    let (start, dead, _) = add_dead_walk_prefix(&mut txn);
    let unresolved = test_pos(8, 13);
    txn.add_edge(dead, EdgeFlags::BLOCK, unresolved);
    txn.fail_find_block(
        unresolved,
        Fault::Inconsistent("dead-walk destination resolution"),
    );

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "dead-walk destination resolution",
    );
}

#[test]
fn alternate_parent_resolution_error_propagates() {
    let mut txn = FaultGraphTxn::default();
    let (start, dead, dead_end) = add_dead_walk_prefix(&mut txn);
    let live_pos = test_pos(3, 0);
    let live = test_node(3, 0, 10);
    let alternate_end = test_pos(5, 10);

    txn.add_edge(dead, EdgeFlags::BLOCK, live_pos);
    txn.resolve_block(live_pos, live);
    txn.add_edge(live, EdgeFlags::PARENT | EdgeFlags::BLOCK, dead_end);
    txn.add_edge(live, EdgeFlags::PARENT | EdgeFlags::BLOCK, alternate_end);
    txn.resolve_block_end(dead_end, dead);
    txn.fail_find_block_end(
        alternate_end,
        Fault::Inconsistent("alternate-parent resolution"),
    );

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "alternate-parent resolution",
    );
}

#[test]
fn alternate_parent_aliveness_error_propagates() {
    let mut txn = FaultGraphTxn::default();
    let (start, dead, dead_end) = add_dead_walk_prefix(&mut txn);
    let live_pos = test_pos(3, 0);
    let live = test_node(3, 0, 10);
    let alternate_end = test_pos(5, 10);
    let alternate = test_node(5, 0, 10);

    txn.add_edge(dead, EdgeFlags::BLOCK, live_pos);
    txn.resolve_block(live_pos, live);
    txn.add_edge(live, EdgeFlags::PARENT | EdgeFlags::BLOCK, dead_end);
    txn.add_edge(live, EdgeFlags::PARENT | EdgeFlags::BLOCK, alternate_end);
    txn.resolve_block_end(dead_end, dead);
    txn.resolve_block_end(alternate_end, alternate);
    txn.fail_parent_item(alternate, Fault::Inconsistent("alternate-parent aliveness"));

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "alternate-parent aliveness",
    );
}

#[test]
fn visible_chain_iterator_error_propagates() {
    let mut txn = FaultGraphTxn::default();
    let (start, first, _) = add_visible_chain_fixture(&mut txn);
    txn.fail_forward_item(first, Fault::Inconsistent("visible-chain iterator"));

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "visible-chain iterator",
    );
}

#[test]
fn visible_chain_resolution_error_propagates() {
    let mut txn = FaultGraphTxn::default();
    let (start, first, _) = add_visible_chain_fixture(&mut txn);
    let unresolved = test_pos(9, 21);
    txn.add_edge(first, EdgeFlags::BLOCK, unresolved);
    txn.fail_find_block(
        unresolved,
        Fault::Inconsistent("visible-chain destination resolution"),
    );

    assert_inconsistent(
        retrieve_graph(&txn, start, RetrieveOptions::new()),
        "visible-chain destination resolution",
    );
}

// -------------------------------------------------------------------------
// RetrieveOptions Tests
// -------------------------------------------------------------------------

#[test]
fn test_retrieve_options_default() {
    let opts = RetrieveOptions::default();
    assert!(!opts.include_deleted);
    assert!(opts.max_vertices.is_none());
    assert!(opts.graph_visibility.is_none());
}

#[test]
fn test_retrieve_options_new() {
    let opts = RetrieveOptions::new();
    assert!(!opts.include_deleted);
    assert!(!opts.has_filter());
}

#[test]
fn test_retrieve_options_include_deleted() {
    let opts = RetrieveOptions::new().include_deleted(true);
    assert!(opts.include_deleted);
}

#[test]
fn test_retrieve_options_max_vertices() {
    let opts = RetrieveOptions::new().max_vertices(100);
    assert_eq!(opts.max_vertices, Some(100));
}

#[test]
fn test_retrieve_options_chaining() {
    let opts = RetrieveOptions::new()
        .include_deleted(true)
        .max_vertices(50);

    assert!(opts.include_deleted);
    assert_eq!(opts.max_vertices, Some(50));
}

#[test]
fn test_include_deleted_edges_default() {
    let opts = RetrieveOptions::default();
    assert!(!opts.include_deleted_edges());
}

#[test]
fn test_include_deleted_edges_explicit() {
    let opts = RetrieveOptions::new().include_deleted(true);
    assert!(opts.include_deleted_edges());
}

#[test]
fn test_include_deleted_edges_with_filter() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);
    // A change filter always forces inclusion of deleted edges
    assert!(opts.include_deleted_edges());
}

// -------------------------------------------------------------------------
// is_edge_alive Tests (typed ForwardEdge model)
// -------------------------------------------------------------------------

fn make_forward_edge(kind: EdgeKind, introduced_by: u64) -> ForwardEdge {
    ForwardEdge {
        kind,
        dest: Position::new(NodeId::new(99), ChangePosition::new(0)),
        introduced_by: NodeId::new(introduced_by),
    }
}

#[test]
fn test_is_edge_alive_no_filter_alive_edge() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::Block, 1);
    assert!(opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_no_filter_deleted_edge() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::BlockDeleted, 1);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_no_filter_folder_edge() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::Folder, 1);
    assert!(opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_no_filter_folder_deleted() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::FolderDeleted, 1);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_no_filter_pseudo_block() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::PseudoBlock, 1);
    assert!(opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_no_filter_pseudo_folder() {
    let opts = RetrieveOptions::new();
    let edge = make_forward_edge(EdgeKind::PseudoFolder, 1);
    assert!(opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_with_filter_alive_edge_in_filter() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);

    let edge = make_forward_edge(EdgeKind::Block, 1);
    assert!(opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_with_filter_deleted_by_in_filter_change() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);

    // Deletion introduced by change 1 which IS in our filter → dead
    let edge = make_forward_edge(EdgeKind::BlockDeleted, 1);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_with_filter_deleted_by_outside_change() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);

    // A `BlockDeleted` edge is NEVER alive as a forward edge — its only
    // role is to flag the original Block edge as deleted (handled via the
    // parent-side check in `is_vertex_alive`).  Even when the deletion's
    // introducer is outside our filter, we still don't follow it as a
    // Block edge: reachability to the destination is provided by the
    // original (separate) Block edge entry.
    let edge = make_forward_edge(EdgeKind::BlockDeleted, 2);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_with_filter_folder_deleted_by_outside_change() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);

    // Same rationale as the BlockDeleted case above.
    let edge = make_forward_edge(EdgeKind::FolderDeleted, 2);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_is_edge_alive_with_filter_folder_deleted_by_in_filter_change() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let opts = RetrieveOptions::new().with_change_filter(filter);

    let edge = make_forward_edge(EdgeKind::FolderDeleted, 1);
    assert!(!opts.is_edge_alive(&edge));
}

#[test]
fn test_retrieve_options_equality() {
    let opts1 = RetrieveOptions::new()
        .include_deleted(true)
        .max_vertices(100);
    let opts2 = RetrieveOptions::new()
        .include_deleted(true)
        .max_vertices(100);

    assert_eq!(opts1, opts2);
}

#[test]
fn test_retrieve_options_clone() {
    let opts1 = RetrieveOptions::new()
        .include_deleted(true)
        .max_vertices(100);
    let opts2 = opts1.clone();

    assert_eq!(opts1, opts2);
}

#[test]
fn test_retrieve_options_debug() {
    let opts = RetrieveOptions::new().include_deleted(true);
    let debug = format!("{:?}", opts);
    assert!(debug.contains("include_deleted"));
}

// Change Filter Tests

#[test]
fn test_retrieve_options_with_graph_visibility() {
    let opts =
        RetrieveOptions::new().with_graph_visibility(visibility([NodeId::new(1), NodeId::new(2)]));
    assert!(opts.has_filter());
    assert!(opts.passes_filter(NodeId::new(1)));
    assert!(opts.passes_filter(NodeId::new(2)));
}

#[test]
fn test_retrieve_options_with_change_filter_arc() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let arc_filter = Arc::new(filter);

    let opts = RetrieveOptions::new().with_change_filter_arc(arc_filter.clone());
    assert!(opts.has_filter());
}

#[test]
fn test_passes_filter_no_filter() {
    let opts = RetrieveOptions::new();

    // Without filter, all should pass
    assert!(opts.passes_filter(NodeId::ROOT));
    assert!(opts.passes_filter(NodeId::new(1)));
    assert!(opts.passes_filter(NodeId::new(100)));
}

#[test]
fn test_passes_filter_root_always_passes() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));

    let opts = RetrieveOptions::new().with_change_filter(filter);

    // ROOT should always pass even if not in filter
    assert!(opts.passes_filter(NodeId::ROOT));
}

#[test]
fn test_passes_filter_in_set() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    filter.insert(NodeId::new(2));

    let opts = RetrieveOptions::new().with_change_filter(filter);

    assert!(opts.passes_filter(NodeId::new(1)));
    assert!(opts.passes_filter(NodeId::new(2)));
}

#[test]
fn test_passes_filter_not_in_set() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));

    let opts = RetrieveOptions::new().with_change_filter(filter);

    assert!(!opts.passes_filter(NodeId::new(2)));
    assert!(!opts.passes_filter(NodeId::new(100)));
}

#[test]
fn test_empty_visibility_is_filtered_but_none_is_ambient() {
    let opts = RetrieveOptions::new().with_graph_visibility(GraphVisibilityClosure::empty());

    // An empty closure is active visibility and means only ROOT passes.
    assert!(opts.passes_filter(NodeId::ROOT));
    assert!(!opts.passes_filter(NodeId::new(1)));
    assert!(!RetrieveOptions::new().has_filter());
    assert!(RetrieveOptions::new().passes_filter(NodeId::new(1)));
}

#[test]
fn test_retrieve_options_equality_with_filter() {
    let mut filter1 = HashSet::new();
    filter1.insert(NodeId::new(1));

    let mut filter2 = HashSet::new();
    filter2.insert(NodeId::new(1));

    let opts1 = RetrieveOptions::new().with_change_filter(filter1);
    let opts2 = RetrieveOptions::new().with_change_filter(filter2);

    // Same contents should be equal
    assert_eq!(opts1, opts2);
}

#[test]
fn test_retrieve_options_equality_different_filters() {
    let mut filter1 = HashSet::new();
    filter1.insert(NodeId::new(1));

    let mut filter2 = HashSet::new();
    filter2.insert(NodeId::new(2));

    let opts1 = RetrieveOptions::new().with_change_filter(filter1);
    let opts2 = RetrieveOptions::new().with_change_filter(filter2);

    // Different contents should not be equal
    assert_ne!(opts1, opts2);
}

#[test]
fn test_retrieve_options_equality_one_with_filter() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));

    let opts1 = RetrieveOptions::new().with_change_filter(filter);
    let opts2 = RetrieveOptions::new();

    // One with filter, one without should not be equal
    assert_ne!(opts1, opts2);
}

#[test]
fn test_retrieve_options_clone_with_filter() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    filter.insert(NodeId::new(2));

    let opts1 = RetrieveOptions::new()
        .include_deleted(true)
        .with_change_filter(filter);
    let opts2 = opts1.clone();

    assert_eq!(opts1, opts2);
    assert!(opts2.has_filter());
    assert!(opts2.passes_filter(NodeId::new(1)));
}

#[test]
fn test_retrieve_options_shared_filter_arc() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    let arc = Arc::new(filter);

    let opts1 = RetrieveOptions::new().with_change_filter_arc(arc.clone());
    let opts2 = RetrieveOptions::new().with_change_filter_arc(arc.clone());

    assert_eq!(opts1.graph_visibility, opts2.graph_visibility);
}

// -------------------------------------------------------------------------
// RetrieveResult Tests
// -------------------------------------------------------------------------

#[test]
fn test_retrieve_result_new() {
    let graph = AliveGraph::new();
    let result = RetrieveResult::new(graph);

    assert!(!result.truncated);
    assert_eq!(result.positions_visited, 0);
    assert_eq!(result.edges_traversed, 0);
}

#[test]
fn test_retrieve_result_debug() {
    let graph = AliveGraph::new();
    let result = RetrieveResult::new(graph);
    let debug = format!("{:?}", result);
    assert!(debug.contains("RetrieveResult"));
}

// -------------------------------------------------------------------------
// Position Helper Tests
// -------------------------------------------------------------------------

#[test]
fn test_inode_vertex_from_position() {
    let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
    let inode_vertex = pos.inode_node();

    assert_eq!(inode_vertex.change, NodeId::new(42));
    assert_eq!(inode_vertex.start, ChangePosition::new(100));
    assert_eq!(inode_vertex.end, ChangePosition::new(100));
    assert!(inode_vertex.is_empty());
}

#[test]
fn test_root_position() {
    let pos = Position::ROOT;
    assert_eq!(pos.change, NodeId::ROOT);
}

#[test]
fn test_bottom_position() {
    let pos = Position::BOTTOM;
    assert_eq!(pos.change, NodeId::ROOT);
}

// -------------------------------------------------------------------------
// Span Classification Tests (Unit Tests Without DB)
// -------------------------------------------------------------------------

#[test]
fn test_alive_vertex_zombie_flag() {
    let node = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(10),
    );
    let mut alive = AliveVertex::new(node);
    assert!(!alive.is_zombie());

    alive.add_flags(VertexFlags::ZOMBIE);
    assert!(alive.is_zombie());
}

#[test]
fn test_alive_vertex_empty() {
    let empty_vertex = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(0),
    );

    let alive = AliveVertex::new(empty_vertex);
    assert!(alive.is_empty());
    assert_eq!(alive.len(), 0);
}

#[test]
fn test_alive_vertex_non_empty() {
    let node = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(50),
    );

    let alive = AliveVertex::new(node);
    assert!(!alive.is_empty());
    assert_eq!(alive.len(), 50);
}

// -------------------------------------------------------------------------
// Edge Flag Tests
// -------------------------------------------------------------------------

#[test]
fn test_parent_flag_detection() {
    let parent_flags = EdgeFlags::PARENT | EdgeFlags::BLOCK;
    assert!(parent_flags.intersects(EdgeFlags::PARENT));
}

#[test]
fn test_deleted_flag_detection() {
    let deleted_flags = EdgeFlags::DELETED | EdgeFlags::BLOCK;
    assert!(deleted_flags.contains(EdgeFlags::DELETED));
}

#[test]
fn test_block_flag_detection() {
    let block_flags = EdgeFlags::BLOCK;
    assert!(block_flags.contains(EdgeFlags::BLOCK));
    assert!(!block_flags.contains(EdgeFlags::DELETED));
}

#[test]
fn test_pseudo_flag_detection() {
    let pseudo_flags = EdgeFlags::PSEUDO;
    assert!(pseudo_flags.contains(EdgeFlags::PSEUDO));
    assert!(!pseudo_flags.contains(EdgeFlags::BLOCK));
}

// -------------------------------------------------------------------------
// Cache Tests (Simulated)
// -------------------------------------------------------------------------

#[test]
fn test_position_cache() {
    let mut cache: HashMap<Position<NodeId>, VertexId> = HashMap::new();

    let pos1 = Position::new(NodeId::new(1), ChangePosition::new(0));
    let pos2 = Position::new(NodeId::new(1), ChangePosition::new(100));
    let pos3 = Position::new(NodeId::new(2), ChangePosition::new(0));

    cache.insert(pos1, VertexId::new(1));
    cache.insert(pos2, VertexId::new(2));
    cache.insert(pos3, VertexId::new(3));

    assert_eq!(cache.get(&pos1), Some(&VertexId::new(1)));
    assert_eq!(cache.get(&pos2), Some(&VertexId::new(2)));
    assert_eq!(cache.get(&pos3), Some(&VertexId::new(3)));

    // Duplicate insertion returns same ID
    assert!(cache.contains_key(&pos1));
}

#[test]
fn test_position_cache_bottom() {
    let mut cache: HashMap<Position<NodeId>, VertexId> = HashMap::new();
    cache.insert(Position::BOTTOM, VertexId::DUMMY);

    assert_eq!(cache.get(&Position::BOTTOM), Some(&VertexId::DUMMY));
}

// -------------------------------------------------------------------------
// Graph Building Tests (Unit Level)
// -------------------------------------------------------------------------

#[test]
fn test_graph_building_basic() {
    let mut graph = AliveGraph::new();

    // Add dummy
    graph.push_vertex(AliveVertex::DUMMY);

    // Add root
    let root = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(0),
    );
    graph.push_vertex(AliveVertex::new(root));

    assert_eq!(graph.len_vertices(), 2);
    assert!(
        graph.get_vertex(VertexId::DUMMY).node.is_root()
            || graph.get_vertex(VertexId::DUMMY).is_dummy()
    );
}

#[test]
fn test_graph_building_with_children() {
    let mut graph = AliveGraph::new();

    // Add dummy
    graph.push_vertex(AliveVertex::DUMMY);

    // Add root with children setup
    let root = GraphNode::ROOT;
    graph.push_vertex(AliveVertex::new(root));
    graph.set_last_children_start();

    // Add children
    graph.push_child_to_last(None, VertexId::new(2));
    graph.push_child_to_last(None, VertexId::DUMMY); // sentinel

    let children: Vec<_> = graph.children(VertexId::new(1)).collect();
    assert_eq!(children.len(), 2);
}

#[test]
fn test_graph_total_bytes() {
    let mut graph = AliveGraph::new();

    graph.push_vertex(AliveVertex::DUMMY);

    let v1 = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(100),
    );
    let v2 = GraphNode::new(
        NodeId::new(2),
        ChangePosition::new(0),
        ChangePosition::new(50),
    );

    graph.push_vertex(AliveVertex::new(v1));
    graph.push_vertex(AliveVertex::new(v2));

    assert_eq!(graph.total_bytes(), 150);
}

// -------------------------------------------------------------------------
// Edge Cases
// -------------------------------------------------------------------------

#[test]
fn test_empty_graph() {
    let graph = AliveGraph::new();
    assert!(graph.is_empty());
    assert_eq!(graph.len_vertices(), 0);
    assert_eq!(graph.total_bytes(), 0);
}

#[test]
fn test_max_vertices_zero() {
    let opts = RetrieveOptions::new().max_vertices(0);
    assert_eq!(opts.max_vertices, Some(0));
}

#[test]
fn test_max_vertices_large() {
    let opts = RetrieveOptions::new().max_vertices(usize::MAX);
    assert_eq!(opts.max_vertices, Some(usize::MAX));
}

#[test]
fn test_retrieve_result_fields() {
    let graph = AliveGraph::new();
    let mut result = RetrieveResult::new(graph);

    result.truncated = true;
    result.positions_visited = 100;
    result.edges_traversed = 500;

    assert!(result.truncated);
    assert_eq!(result.positions_visited, 100);
    assert_eq!(result.edges_traversed, 500);
}

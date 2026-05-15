use super::super::graph::AliveGraph;
use super::super::vertex::{AliveVertex, VertexFlags, VertexId};
use super::options::{RetrieveOptions, RetrieveResult};
use crate::types::{ChangePosition, EdgeFlags, EdgeKind, ForwardEdge, GraphNode, NodeId, Position};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// -------------------------------------------------------------------------
// RetrieveOptions Tests
// -------------------------------------------------------------------------

#[test]
fn test_retrieve_options_default() {
    let opts = RetrieveOptions::default();
    assert!(!opts.include_deleted);
    assert!(opts.max_vertices.is_none());
    assert!(opts.change_filter.is_none());
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
fn test_retrieve_options_with_change_filter() {
    let mut filter = HashSet::new();
    filter.insert(NodeId::new(1));
    filter.insert(NodeId::new(2));

    let opts = RetrieveOptions::new().with_change_filter(filter);
    assert!(opts.has_filter());
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
fn test_passes_filter_empty_set() {
    let filter: HashSet<NodeId> = HashSet::new();
    let opts = RetrieveOptions::new().with_change_filter(filter);

    // Empty filter means only ROOT passes
    assert!(opts.passes_filter(NodeId::ROOT));
    assert!(!opts.passes_filter(NodeId::new(1)));
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

    // Both should reference the same Arc
    assert!(Arc::ptr_eq(
        opts1.change_filter.as_ref().unwrap(),
        opts2.change_filter.as_ref().unwrap()
    ));
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

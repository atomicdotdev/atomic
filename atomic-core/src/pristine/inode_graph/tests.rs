use super::*;
use crate::types::{ChangePosition, EdgeFlags, GraphNode, Inode, NodeId};
// -------------------------------------------------------------------------
// InodeVertex Tests
// -------------------------------------------------------------------------

#[test]
fn test_inode_vertex_new() {
    let inode = Inode::new(42);
    let node = GraphNode::new(
        NodeId::new(1),
        ChangePosition::new(0),
        ChangePosition::new(100),
    );
    let iv = InodeVertex::new(inode, node);

    assert_eq!(iv.inode, inode);
    assert_eq!(iv.node, node);
}

#[test]
fn test_inode_vertex_root() {
    assert!(InodeVertex::ROOT.is_root());
    assert_eq!(InodeVertex::ROOT.inode, Inode::ROOT);
    assert_eq!(InodeVertex::ROOT.node, GraphNode::ROOT);
}

#[test]
fn test_inode_vertex_min_for_inode() {
    let inode = Inode::new(99);
    let iv = InodeVertex::min_for_inode(inode);

    assert_eq!(iv.inode, inode);
    assert_eq!(iv.node, GraphNode::ROOT);
}

#[test]
fn test_inode_vertex_max_for_inode() {
    let inode = Inode::new(99);
    let iv = InodeVertex::max_for_inode(inode);

    assert_eq!(iv.inode, inode);
    assert_eq!(iv.node, GraphNode::MAX);
}

#[test]
fn test_inode_vertex_ordering() {
    let iv1 = InodeVertex::new(
        Inode::new(1),
        GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        ),
    );
    let iv2 = InodeVertex::new(
        Inode::new(1),
        GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(10),
        ),
    );
    let iv3 = InodeVertex::new(
        Inode::new(2),
        GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        ),
    );

    // Same inode, different span - ordered by span
    assert!(iv1 < iv2);

    // Different inode - ordered by inode first
    assert!(iv1 < iv3);
    assert!(iv2 < iv3);
}

#[test]
fn test_inode_vertex_positions() {
    let inode = Inode::new(1);
    let node = GraphNode::new(
        NodeId::new(5),
        ChangePosition::new(10),
        ChangePosition::new(20),
    );
    let iv = InodeVertex::new(inode, node);

    let start = iv.start_pos();
    assert_eq!(start.change, NodeId::new(5));
    assert_eq!(start.pos, ChangePosition::new(10));

    let end = iv.end_pos();
    assert_eq!(end.change, NodeId::new(5));
    assert_eq!(end.pos, ChangePosition::new(20));
}

#[test]
fn test_inode_vertex_display() {
    let iv = InodeVertex::new(
        Inode::new(42),
        GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        ),
    );
    let display = iv.to_string();
    assert!(display.contains("IV"));
    assert!(display.contains("42"));
}

#[test]
fn test_inode_vertex_debug() {
    let iv = InodeVertex::ROOT;
    let debug = format!("{:?}", iv);
    assert!(debug.contains("InodeVertex"));
}

#[test]
fn test_inode_vertex_hash() {
    use std::collections::HashSet;
    let iv1 = InodeVertex::new(Inode::new(1), GraphNode::ROOT);
    let iv2 = InodeVertex::new(Inode::new(2), GraphNode::ROOT);
    let iv3 = InodeVertex::new(Inode::new(1), GraphNode::ROOT); // duplicate

    let mut set = HashSet::new();
    set.insert(iv1);
    set.insert(iv2);
    set.insert(iv3);
    assert_eq!(set.len(), 2);
}

// -------------------------------------------------------------------------
// InodeAdjState Tests
// -------------------------------------------------------------------------

#[test]
fn test_inode_adj_state_new() {
    let state = InodeAdjState::new(
        Inode::new(1),
        GraphNode::ROOT,
        EdgeFlags::empty(),
        EdgeFlags::BLOCK,
    );

    assert_eq!(state.inode, Inode::new(1));
    assert_eq!(state.position, 0);
    assert!(!state.is_exhausted());
}

#[test]
fn test_inode_adj_state_advance() {
    let mut state = InodeAdjState::new(
        Inode::new(1),
        GraphNode::ROOT,
        EdgeFlags::empty(),
        EdgeFlags::BLOCK,
    );

    assert_eq!(state.position, 0);
    state.advance();
    assert_eq!(state.position, 1);
    state.advance();
    assert_eq!(state.position, 2);
}

#[test]
fn test_inode_adj_state_exhausted() {
    let mut state = InodeAdjState::new(
        Inode::new(1),
        GraphNode::ROOT,
        EdgeFlags::empty(),
        EdgeFlags::BLOCK,
    );

    assert!(!state.is_exhausted());
    state.mark_exhausted();
    assert!(state.is_exhausted());
}

#[test]
fn test_inode_adj_state_clone() {
    let state = InodeAdjState::new(
        Inode::new(42),
        GraphNode::ROOT,
        EdgeFlags::BLOCK,
        EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
    );

    let cloned = state.clone();
    assert_eq!(state.inode, cloned.inode);
    assert_eq!(state.min_flag, cloned.min_flag);
    assert_eq!(state.max_flag, cloned.max_flag);
}

// -------------------------------------------------------------------------
// InodeGraphStats Tests
// -------------------------------------------------------------------------

#[test]
fn test_inode_graph_stats_new() {
    let stats = InodeGraphStats::new();
    assert_eq!(stats.vertices_visited, 0);
    assert_eq!(stats.edges_traversed, 0);
    assert_eq!(stats.page_accesses, 0);
    assert_eq!(stats.cache_hits, 0);
}

#[test]
fn test_inode_graph_stats_default() {
    let stats = InodeGraphStats::default();
    assert_eq!(stats, InodeGraphStats::new());
}

#[test]
fn test_inode_graph_stats_merge() {
    let mut s1 = InodeGraphStats {
        vertices_visited: 10,
        edges_traversed: 20,
        page_accesses: 5,
        cache_hits: 3,
    };

    let s2 = InodeGraphStats {
        vertices_visited: 5,
        edges_traversed: 10,
        page_accesses: 2,
        cache_hits: 1,
    };

    s1.merge(&s2);

    assert_eq!(s1.vertices_visited, 15);
    assert_eq!(s1.edges_traversed, 30);
    assert_eq!(s1.page_accesses, 7);
    assert_eq!(s1.cache_hits, 4);
}

#[test]
fn test_inode_graph_stats_cache_hit_ratio() {
    let stats = InodeGraphStats {
        vertices_visited: 80,
        edges_traversed: 0,
        page_accesses: 0,
        cache_hits: 20,
    };

    // 20 / (80 + 20) = 0.2
    assert!((stats.cache_hit_ratio() - 0.2).abs() < 0.001);
}

#[test]
fn test_inode_graph_stats_cache_hit_ratio_empty() {
    let stats = InodeGraphStats::new();
    assert_eq!(stats.cache_hit_ratio(), 0.0);
}

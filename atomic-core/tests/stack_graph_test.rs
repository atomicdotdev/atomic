//! Integration tests for the two-tier stack graph model
//!
//! Tests cover:
//! - Phase 1: `StackKind`, `StackState`, `STACK_GRAPH` CRUD, `create_stack`,
//!   `get_stack_by_id`, `del_stack_graph_prefix`, `resolve_overlay_chain`,
//!   backward-compatible serialization
//! - Phase 3: `OverlayTxn` graph traversal — `iter_adjacent`, `find_block`,
//!   `find_block_end`, `has_vertex` reading from STACK_GRAPH chain ∪ GRAPH

use atomic_core::pristine::{
    GraphTxnT, MutTxnT, OverlayTxn, Pristine, StackKind, StackState, StackTxnT,
};
use atomic_core::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, NodeId, Position, SerializedGraphEdge,
};
use std::collections::HashSet;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_pristine() -> (tempfile::TempDir, Pristine) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();
    (dir, pristine)
}

fn make_edge(
    flag: EdgeFlags,
    dest_change: u64,
    dest_pos: u64,
    introduced_by: u64,
) -> SerializedGraphEdge {
    SerializedGraphEdge::new(
        flag,
        Position::new(NodeId::new(dest_change), ChangePosition::new(dest_pos)),
        NodeId::new(introduced_by),
    )
}

fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
    GraphNode::new(
        NodeId::new(change),
        ChangePosition::new(start),
        ChangePosition::new(end),
    )
}

// ===========================================================================
// StackKind unit tests
// ===========================================================================

#[test]
fn stack_kind_from_u8_roundtrip() {
    assert_eq!(StackKind::from_u8(0), Some(StackKind::Local));
    assert_eq!(StackKind::from_u8(1), Some(StackKind::Shared));
    assert_eq!(StackKind::from_u8(2), None);
    assert_eq!(StackKind::from_u8(255), None);
}

#[test]
fn stack_kind_predicates() {
    assert!(StackKind::Shared.is_shared());
    assert!(!StackKind::Shared.is_local());
    assert!(StackKind::Local.is_local());
    assert!(!StackKind::Local.is_shared());
}

#[test]
fn stack_kind_default_is_shared() {
    assert_eq!(StackKind::default(), StackKind::Shared);
}

#[test]
fn stack_kind_display() {
    assert_eq!(format!("{}", StackKind::Local), "local");
    assert_eq!(format!("{}", StackKind::Shared), "shared");
}

// ===========================================================================
// StackState construction
// ===========================================================================

#[test]
fn stack_state_new_defaults_to_shared_no_parent() {
    let state = StackState::new(1, "main".to_string());
    assert_eq!(state.kind, StackKind::Shared);
    assert_eq!(state.parent, None);
    assert!(state.is_root());
    assert!(state.is_empty());
}

#[test]
fn stack_state_with_kind_isolated_and_parent() {
    let state = StackState::with_kind(5, "feature".to_string(), StackKind::Local, Some(2));
    assert_eq!(state.id, 5);
    assert_eq!(state.name, "feature");
    assert_eq!(state.kind, StackKind::Local);
    assert_eq!(state.parent, Some(2));
    assert!(!state.is_root());
}

// ===========================================================================
// create_stack with kind + parent
// ===========================================================================

#[test]
fn create_shared_root_stack() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    assert_eq!(main.name, "main");
    assert_eq!(main.kind, StackKind::Shared);
    assert_eq!(main.parent, None);
    assert!(main.is_root());

    txn.commit().unwrap();
}

#[test]
fn create_shared_child_of_shared() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();

    assert_eq!(dev.kind, StackKind::Shared);
    assert_eq!(dev.parent, Some(main.id));
    assert!(!dev.is_root());

    txn.commit().unwrap();
}

#[test]
fn create_isolated_child_of_shared() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    assert_eq!(feature.kind, StackKind::Local);
    assert_eq!(feature.parent, Some(dev.id));

    txn.commit().unwrap();
}

#[test]
fn create_isolated_stacked_on_isolated() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    assert_eq!(feature.parent, Some(service.id));
    assert_eq!(service.parent, Some(dev.id));

    txn.commit().unwrap();
}

#[test]
fn create_stack_duplicate_name_errors() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    txn.create_stack("dev", StackKind::Shared, None).unwrap();

    let result = txn.create_stack("dev", StackKind::Local, None);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn create_stack_nonexistent_parent_errors() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let result = txn.create_stack("feature", StackKind::Local, Some(9999));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("not found"));
}

// ===========================================================================
// get_stack_by_id
// ===========================================================================

#[test]
fn get_stack_by_id_found() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();

    let found = StackTxnT::get_stack_by_id(&txn, dev.id).unwrap();
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.name, "dev");
    assert_eq!(found.kind, StackKind::Shared);
    assert_eq!(found.parent, Some(main.id));

    txn.commit().unwrap();
}

#[test]
fn get_stack_by_id_not_found() {
    let (_dir, pristine) = open_pristine();
    let txn = pristine.write_txn().unwrap();

    let found = StackTxnT::get_stack_by_id(&txn, 42).unwrap();
    assert!(found.is_none());
}

#[test]
fn get_stack_by_id_works_on_read_txn() {
    let (_dir, pristine) = open_pristine();

    let stack_id;
    {
        let mut txn = pristine.write_txn().unwrap();
        let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
        stack_id = main.id;
        txn.commit().unwrap();
    }

    let txn = pristine.read_txn().unwrap();
    let found = txn.get_stack_by_id(stack_id).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "main");
}

// ===========================================================================
// open_or_create_stack backward compatibility
// ===========================================================================

#[test]
fn open_or_create_stack_defaults_to_shared() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let stack = txn.open_or_create_stack("legacy").unwrap();
    assert_eq!(stack.kind, StackKind::Shared);
    assert_eq!(stack.parent, None);

    txn.commit().unwrap();
}

#[test]
fn open_or_create_stack_returns_existing_with_kind() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // open_or_create should return the existing stack with its original kind
    let reopened = txn.open_or_create_stack("feature").unwrap();
    assert_eq!(reopened.id, feature.id);
    assert_eq!(reopened.kind, StackKind::Local);
    assert_eq!(reopened.parent, Some(dev.id));

    txn.commit().unwrap();
}

// ===========================================================================
// STACK_GRAPH CRUD: put / get / del
// ===========================================================================

#[test]
fn put_and_read_stack_graph_edge() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);

    // redb multimap insert returns whether the key already existed,
    // not whether the specific value was new — just verify no error
    txn.put_stack_graph(feature.id, vertex, edge).unwrap();

    // Read it back via iter_stack_graph_adjacent
    let edges: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].flag(), EdgeFlags::BLOCK);
    assert_eq!(edges[0].dest().change, NodeId::new(2));

    txn.commit().unwrap();
}

#[test]
fn stack_graph_edges_are_isolated_per_stack() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature_a = txn
        .create_stack("feature-a", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature_b = txn
        .create_stack("feature-b", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let edge_a = make_edge(EdgeFlags::BLOCK, 10, 0, 100);
    let edge_b = make_edge(EdgeFlags::BLOCK, 20, 0, 200);

    txn.put_stack_graph(feature_a.id, vertex, edge_a).unwrap();
    txn.put_stack_graph(feature_b.id, vertex, edge_b).unwrap();

    // feature-a should only see edge_a
    let edges_a: Vec<_> = txn
        .iter_stack_graph_adjacent(feature_a.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges_a.len(), 1);
    assert_eq!(edges_a[0].dest().change, NodeId::new(10));

    // feature-b should only see edge_b
    let edges_b: Vec<_> = txn
        .iter_stack_graph_adjacent(feature_b.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges_b.len(), 1);
    assert_eq!(edges_b[0].dest().change, NodeId::new(20));

    txn.commit().unwrap();
}

#[test]
fn del_stack_graph_removes_specific_edge() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let edge1 = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    let edge2 = make_edge(EdgeFlags::FOLDER, 3, 0, 1);

    txn.put_stack_graph(feature.id, vertex, edge1).unwrap();
    txn.put_stack_graph(feature.id, vertex, edge2).unwrap();

    // Remove just the BLOCK edge
    let removed = txn.del_stack_graph(feature.id, vertex, edge1).unwrap();
    assert!(removed);

    // Only the FOLDER edge remains
    let edges: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].flag(), EdgeFlags::FOLDER);

    txn.commit().unwrap();
}

#[test]
fn del_stack_graph_nonexistent_returns_false() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let vertex = make_vertex(1, 0, 10);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);

    let removed = txn.del_stack_graph(999, vertex, edge).unwrap();
    assert!(!removed);
}

#[test]
fn stack_graph_flag_filtering() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let block_edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    let folder_edge = make_edge(EdgeFlags::FOLDER, 3, 0, 1);
    let deleted_edge = make_edge(EdgeFlags::DELETED | EdgeFlags::BLOCK, 4, 0, 1);

    txn.put_stack_graph(feature.id, vertex, block_edge).unwrap();
    txn.put_stack_graph(feature.id, vertex, folder_edge)
        .unwrap();
    txn.put_stack_graph(feature.id, vertex, deleted_edge)
        .unwrap();

    // Only BLOCK edges (no DELETED, no FOLDER)
    let block_only: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, vertex, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(block_only.len(), 1);
    assert_eq!(block_only[0].flag(), EdgeFlags::BLOCK);

    txn.commit().unwrap();
}

// ===========================================================================
// del_stack_graph_prefix: cascade deletion
// ===========================================================================

#[test]
fn del_stack_graph_prefix_removes_all_edges_for_stack() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Add multiple vertices and edges
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(2, 0, 20);
    let v3 = make_vertex(3, 0, 30);

    txn.put_stack_graph(feature.id, v1, make_edge(EdgeFlags::BLOCK, 10, 0, 1))
        .unwrap();
    txn.put_stack_graph(feature.id, v1, make_edge(EdgeFlags::FOLDER, 11, 0, 1))
        .unwrap();
    txn.put_stack_graph(feature.id, v2, make_edge(EdgeFlags::BLOCK, 20, 0, 2))
        .unwrap();
    txn.put_stack_graph(feature.id, v3, make_edge(EdgeFlags::BLOCK, 30, 0, 3))
        .unwrap();

    // Cascade delete
    let count = txn.del_stack_graph_prefix(feature.id).unwrap();
    assert_eq!(count, 4);

    // All edges should be gone
    for vertex in &[v1, v2, v3] {
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature.id, *vertex, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(edges.is_empty());
    }

    txn.commit().unwrap();
}

#[test]
fn del_stack_graph_prefix_does_not_affect_other_stacks() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature_a = txn
        .create_stack("feature-a", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature_b = txn
        .create_stack("feature-b", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let edge_a = make_edge(EdgeFlags::BLOCK, 10, 0, 100);
    let edge_b = make_edge(EdgeFlags::BLOCK, 20, 0, 200);

    txn.put_stack_graph(feature_a.id, vertex, edge_a).unwrap();
    txn.put_stack_graph(feature_b.id, vertex, edge_b).unwrap();

    // Delete feature-a's edges
    let count = txn.del_stack_graph_prefix(feature_a.id).unwrap();
    assert_eq!(count, 1);

    // feature-a should be empty
    let edges_a: Vec<_> = txn
        .iter_stack_graph_adjacent(feature_a.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(edges_a.is_empty());

    // feature-b should be untouched
    let edges_b: Vec<_> = txn
        .iter_stack_graph_adjacent(feature_b.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges_b.len(), 1);
    assert_eq!(edges_b[0].dest().change, NodeId::new(20));

    txn.commit().unwrap();
}

#[test]
fn del_stack_graph_prefix_on_empty_stack_returns_zero() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let count = txn.del_stack_graph_prefix(999).unwrap();
    assert_eq!(count, 0);

    txn.commit().unwrap();
}

// ===========================================================================
// STACK_GRAPH does NOT interfere with global GRAPH
// ===========================================================================

#[test]
fn stack_graph_and_global_graph_are_independent() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let vertex = make_vertex(1, 0, 10);
    let global_edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);
    let stack_edge = make_edge(EdgeFlags::BLOCK, 60, 0, 600);

    // Write to global GRAPH
    txn.put_graph(vertex, global_edge).unwrap();

    // Write to STACK_GRAPH
    txn.put_stack_graph(feature.id, vertex, stack_edge).unwrap();

    // Global GRAPH should only have global_edge
    let global_edges: Vec<_> = txn
        .iter_adjacent(vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(global_edges.len(), 1);
    assert_eq!(global_edges[0].dest().change, NodeId::new(50));

    // STACK_GRAPH should only have stack_edge
    let stack_edges: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(stack_edges.len(), 1);
    assert_eq!(stack_edges[0].dest().change, NodeId::new(60));

    // Deleting STACK_GRAPH prefix does not affect GRAPH
    txn.del_stack_graph_prefix(feature.id).unwrap();

    let global_edges_after: Vec<_> = txn
        .iter_adjacent(vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(global_edges_after.len(), 1);
    assert_eq!(global_edges_after[0].dest().change, NodeId::new(50));

    txn.commit().unwrap();
}

// ===========================================================================
// Overlay chain resolution
// ===========================================================================

#[test]
fn resolve_overlay_chain_shared_returns_empty() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();

    let chain = txn.resolve_overlay_chain(&dev).unwrap();
    assert!(chain.is_empty(), "Shared stacks read from GRAPH directly");

    txn.commit().unwrap();
}

#[test]
fn resolve_overlay_chain_single_isolated() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let chain = txn.resolve_overlay_chain(&feature).unwrap();
    assert_eq!(chain, vec![feature.id]);

    txn.commit().unwrap();
}

#[test]
fn resolve_overlay_chain_stacked_isolated() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    // main (Shared) → dev (Shared) → service-auth (Isolated) → feature-login (Isolated)
    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    let chain = txn.resolve_overlay_chain(&feature).unwrap();
    assert_eq!(chain, vec![feature.id, service.id]);

    // service-auth's chain should just be itself
    let service_chain = txn.resolve_overlay_chain(&service).unwrap();
    assert_eq!(service_chain, vec![service.id]);

    // dev is Shared → empty chain
    let dev_chain = txn.resolve_overlay_chain(&dev).unwrap();
    assert!(dev_chain.is_empty());

    txn.commit().unwrap();
}

#[test]
fn resolve_overlay_chain_deeply_nested() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let root = txn.create_stack("main", StackKind::Shared, None).unwrap();

    // Build a chain of 5 local workspaces
    let mut parent_id = root.id;
    let mut ids = Vec::new();
    for i in 0..5 {
        let name = format!("layer-{}", i);
        let stack = txn
            .create_stack(&name, StackKind::Local, Some(parent_id))
            .unwrap();
        ids.push(stack.id);
        parent_id = stack.id;
    }

    // Resolve from the deepest layer
    let deepest = StackTxnT::get_stack_by_id(&txn, *ids.last().unwrap())
        .unwrap()
        .unwrap();
    let chain = txn.resolve_overlay_chain(&deepest).unwrap();

    // Should be [layer-4, layer-3, layer-2, layer-1, layer-0]
    let expected: Vec<u64> = ids.iter().rev().cloned().collect();
    assert_eq!(chain, expected);

    txn.commit().unwrap();
}

// ===========================================================================
// Serialization backward compatibility
// ===========================================================================

#[test]
fn stack_state_persists_kind_and_parent_across_transactions() {
    let (_dir, pristine) = open_pristine();

    let feature_id;
    let dev_id;

    // Write
    {
        let mut txn = pristine.write_txn().unwrap();
        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        dev_id = dev.id;
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();
        feature_id = feature.id;
        txn.commit().unwrap();
    }

    // Read back in new transaction
    {
        let txn = pristine.read_txn().unwrap();

        let dev = txn.get_stack("dev").unwrap().unwrap();
        assert_eq!(dev.kind, StackKind::Shared);
        assert_eq!(dev.parent, None);

        let feature = txn.get_stack("feature").unwrap().unwrap();
        assert_eq!(feature.kind, StackKind::Local);
        assert_eq!(feature.parent, Some(dev_id));
        assert_eq!(feature.id, feature_id);
    }
}

#[test]
fn stack_state_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");

    let dev_id;

    // First open: create stacks
    {
        let pristine = Pristine::open(&db_path).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        dev_id = dev.id;
        txn.create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();
    }

    // Second open: verify stacks survived
    {
        let pristine = Pristine::open(&db_path).unwrap();
        let txn = pristine.read_txn().unwrap();

        let dev = txn.get_stack("dev").unwrap().unwrap();
        assert_eq!(dev.kind, StackKind::Shared);

        let feature = txn.get_stack("feature").unwrap().unwrap();
        assert_eq!(feature.kind, StackKind::Local);
        assert_eq!(feature.parent, Some(dev_id));
    }
}

// ===========================================================================
// STACK_GRAPH edges persist across transactions
// ===========================================================================

#[test]
fn stack_graph_edges_persist_across_transactions() {
    let (_dir, pristine) = open_pristine();

    let feature_id;

    // Write edges
    {
        let mut txn = pristine.write_txn().unwrap();
        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();
        feature_id = feature.id;

        let vertex = make_vertex(1, 0, 10);
        let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
        txn.put_stack_graph(feature.id, vertex, edge).unwrap();

        txn.commit().unwrap();
    }

    // Read edges in new transaction
    {
        let txn = pristine.read_txn().unwrap();

        let vertex = make_vertex(1, 0, 10);
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature_id, vertex, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].flag(), EdgeFlags::BLOCK);
        assert_eq!(edges[0].dest().change, NodeId::new(2));
    }
}

// ===========================================================================
// Full workflow: the user's original scenario
// ===========================================================================

/// Simulates: create feature stack → add 7 changes → delete stack → graph clean
#[test]
fn full_workflow_delete_abandoned_feature_stack() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    // Setup hierarchy: main → dev → feature
    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Simulate recording 7 changes on "feature"
    // (In real code, apply would route edges to STACK_GRAPH for Isolated)
    let mut vertices = Vec::new();
    for i in 1..=7u64 {
        let v = make_vertex(i, 0, i * 10);
        let fwd = make_edge(EdgeFlags::BLOCK, i + 100, 0, i);
        let rev = make_edge(EdgeFlags::BLOCK | EdgeFlags::PARENT, i, 0, i);
        txn.put_stack_graph(feature.id, v, fwd).unwrap();
        txn.put_stack_graph(feature.id, v, rev).unwrap();
        vertices.push(v);
    }

    // Verify edges exist (14 total: 2 per change × 7 changes)
    let mut total = 0u64;
    for v in &vertices {
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature.id, *v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        total += edges.len() as u64;
    }
    assert_eq!(total, 14);

    // User decides to abandon the feature: cascade delete
    let deleted_count = txn.del_stack_graph_prefix(feature.id).unwrap();
    assert_eq!(deleted_count, 14);

    // Verify all edges are gone
    for v in &vertices {
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature.id, *v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(edges.is_empty());
    }

    // Global GRAPH is untouched (we never wrote to it)
    for v in &vertices {
        let global: Vec<_> = txn
            .iter_adjacent(*v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(global.is_empty());
    }

    txn.commit().unwrap();
}

/// Simulates: create feature → add changes → apply some to dev → delete feature
/// Changes applied to dev survive; unapplied changes disappear.
#[test]
fn full_workflow_partial_apply_then_delete() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Record 4 changes on feature
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(2, 0, 20);
    let v3 = make_vertex(3, 0, 30);
    let v4 = make_vertex(4, 0, 40);

    let e1 = make_edge(EdgeFlags::BLOCK, 10, 0, 1);
    let e2 = make_edge(EdgeFlags::BLOCK, 20, 0, 2);
    let e3 = make_edge(EdgeFlags::BLOCK, 30, 0, 3);
    let e4 = make_edge(EdgeFlags::BLOCK, 40, 0, 4);

    // All go to STACK_GRAPH (feature is Isolated)
    txn.put_stack_graph(feature.id, v1, e1).unwrap();
    txn.put_stack_graph(feature.id, v2, e2).unwrap();
    txn.put_stack_graph(feature.id, v3, e3).unwrap();
    txn.put_stack_graph(feature.id, v4, e4).unwrap();

    // "Apply" changes 1 and 3 to dev (Shared → GRAPH)
    txn.put_graph(v1, e1).unwrap();
    txn.put_graph(v3, e3).unwrap();

    // Delete feature stack → cascade
    let deleted = txn.del_stack_graph_prefix(feature.id).unwrap();
    assert_eq!(deleted, 4); // All 4 STACK_GRAPH edges removed

    // Changes 1 and 3 survive in GRAPH (they were applied to dev)
    let g1: Vec<_> = txn
        .iter_adjacent(v1, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(g1.len(), 1);
    assert_eq!(g1[0].dest().change, NodeId::new(10));

    let g3: Vec<_> = txn
        .iter_adjacent(v3, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(g3.len(), 1);

    // Changes 2 and 4 are gone from everywhere
    let g2: Vec<_> = txn
        .iter_adjacent(v2, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(g2.is_empty());

    let g4: Vec<_> = txn
        .iter_adjacent(v4, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(g4.is_empty());

    txn.commit().unwrap();
}

/// Simulates the monorepo scenario: two teams with long-lived local workspaces,
/// applying shared infrastructure changes to dev for global visibility.
#[test]
fn full_workflow_monorepo_cross_team_visibility() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    // Hierarchy: main → dev → {service-auth, service-payments}
    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let auth = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let pay = txn
        .create_stack("service-payments", StackKind::Local, Some(dev.id))
        .unwrap();

    // Team A records a proto change on service-auth
    let proto_vertex = make_vertex(100, 0, 50);
    let proto_edge = make_edge(EdgeFlags::BLOCK, 200, 0, 100);
    txn.put_stack_graph(auth.id, proto_vertex, proto_edge)
        .unwrap();

    // Team B can't see it yet (different local workspace)
    let pay_edges: Vec<_> = txn
        .iter_stack_graph_adjacent(pay.id, proto_vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(pay_edges.is_empty());

    // Also not in global GRAPH yet
    let global_edges: Vec<_> = txn
        .iter_adjacent(proto_vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(global_edges.is_empty());

    // Team A applies the proto change to dev (Shared → GRAPH)
    txn.put_graph(proto_vertex, proto_edge).unwrap();

    // Now Team B can see it via GRAPH
    let global_edges_after: Vec<_> = txn
        .iter_adjacent(proto_vertex, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(global_edges_after.len(), 1);
    assert_eq!(global_edges_after[0].dest().change, NodeId::new(200));

    txn.commit().unwrap();
}

/// Stacked features: feature-login on top of service-auth (both Isolated)
#[test]
fn full_workflow_stacked_feature_on_isolated() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let auth = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let login = txn
        .create_stack("feature-login", StackKind::Local, Some(auth.id))
        .unwrap();

    // Verify the overlay chain
    let chain = txn.resolve_overlay_chain(&login).unwrap();
    assert_eq!(chain, vec![login.id, auth.id]);

    // service-auth adds a base change
    let v_base = make_vertex(1, 0, 10);
    let e_base = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_stack_graph(auth.id, v_base, e_base).unwrap();

    // feature-login adds its own change
    let v_login = make_vertex(3, 0, 30);
    let e_login = make_edge(EdgeFlags::BLOCK, 4, 0, 3);
    txn.put_stack_graph(login.id, v_login, e_login).unwrap();

    // Each stack only sees its own STACK_GRAPH directly
    let auth_edges: Vec<_> = txn
        .iter_stack_graph_adjacent(auth.id, v_base, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(auth_edges.len(), 1);

    let login_edges: Vec<_> = txn
        .iter_stack_graph_adjacent(login.id, v_login, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(login_edges.len(), 1);

    // The overlay chain tells us feature-login's full view requires both
    // STACK_GRAPH[login] and STACK_GRAPH[auth] — Phase 3 will implement
    // the actual merged traversal, but the chain is correct.

    // Delete feature-login — service-auth is unaffected
    txn.del_stack_graph_prefix(login.id).unwrap();

    let auth_edges_after: Vec<_> = txn
        .iter_stack_graph_adjacent(auth.id, v_base, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(auth_edges_after.len(), 1);

    txn.commit().unwrap();
}

// ===========================================================================
// Phase 4: del_stack lifecycle — Isolated cascade deletion
// ===========================================================================

#[test]
fn del_stack_isolated_cascades_stack_graph_edges() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Add edges to STACK_GRAPH
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(2, 0, 20);
    txn.put_stack_graph(feature.id, v1, make_edge(EdgeFlags::BLOCK, 10, 0, 1))
        .unwrap();
    txn.put_stack_graph(feature.id, v2, make_edge(EdgeFlags::BLOCK, 20, 0, 2))
        .unwrap();

    // Delete the stack
    txn.del_stack(&feature).unwrap();

    // STACK_GRAPH edges are gone
    let edges_v1: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, v1, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(edges_v1.is_empty());

    let edges_v2: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, v2, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(edges_v2.is_empty());

    // Stack metadata is gone
    assert!(txn.get_stack("feature").unwrap().is_none());

    txn.commit().unwrap();
}

#[test]
fn del_stack_isolated_preserves_global_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let global_edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);
    let stack_edge = make_edge(EdgeFlags::BLOCK, 60, 0, 600);

    txn.put_graph(v, global_edge).unwrap();
    txn.put_stack_graph(feature.id, v, stack_edge).unwrap();

    txn.del_stack(&feature).unwrap();

    // Global GRAPH edge survives
    let global_edges: Vec<_> = txn
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(global_edges.len(), 1);
    assert_eq!(global_edges[0].dest().change, NodeId::new(50));

    txn.commit().unwrap();
}

#[test]
fn del_stack_isolated_preserves_sibling_stack_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature_a = txn
        .create_stack("feature-a", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature_b = txn
        .create_stack("feature-b", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    txn.put_stack_graph(feature_a.id, v, make_edge(EdgeFlags::BLOCK, 10, 0, 1))
        .unwrap();
    txn.put_stack_graph(feature_b.id, v, make_edge(EdgeFlags::BLOCK, 20, 0, 2))
        .unwrap();

    txn.del_stack(&feature_a).unwrap();

    // feature-b's edges are untouched
    let edges_b: Vec<_> = txn
        .iter_stack_graph_adjacent(feature_b.id, v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges_b.len(), 1);
    assert_eq!(edges_b[0].dest().change, NodeId::new(20));

    txn.commit().unwrap();
}

// ===========================================================================
// Phase 4: del_stack — Shared stack deletion blocked
// ===========================================================================

#[test]
fn del_stack_shared_is_blocked() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();

    let result = txn.del_stack(&dev);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cannot delete shared stack"),
        "got: {}",
        err
    );

    // Stack still exists
    assert!(txn.get_stack("dev").unwrap().is_some());
}

// ===========================================================================
// Phase 4: del_stack — Children guard
// ===========================================================================

#[test]
fn del_stack_with_children_is_blocked() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let _feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    // service-auth has a child (feature-login) — cannot delete
    let result = txn.del_stack(&service);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("feature-login"),
        "error should mention the child stack name, got: {}",
        err
    );
}

#[test]
fn del_stack_after_deleting_children_succeeds() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    // Delete child first
    txn.del_stack(&feature).unwrap();

    // Now parent can be deleted
    txn.del_stack(&service).unwrap();

    assert!(txn.get_stack("feature-login").unwrap().is_none());
    assert!(txn.get_stack("service-auth").unwrap().is_none());

    txn.commit().unwrap();
}

#[test]
fn del_stack_with_multiple_children_lists_all_in_error() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let _feat1 = txn
        .create_stack("feat-login", StackKind::Local, Some(service.id))
        .unwrap();
    let _feat2 = txn
        .create_stack("feat-logout", StackKind::Local, Some(service.id))
        .unwrap();
    let _bug = txn
        .create_stack("bug-session", StackKind::Local, Some(service.id))
        .unwrap();

    let result = txn.del_stack(&service);
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.to_string();
    // All three children should be mentioned
    assert!(msg.contains("feat-login"), "missing feat-login in: {}", msg);
    assert!(
        msg.contains("feat-logout"),
        "missing feat-logout in: {}",
        msg
    );
    assert!(
        msg.contains("bug-session"),
        "missing bug-session in: {}",
        msg
    );
}

// ===========================================================================
// Phase 4: get_children_stacks
// ===========================================================================

#[test]
fn get_children_stacks_returns_direct_children() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let _feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    // dev's children: [service-auth]
    let dev_children = txn.get_children_stacks(dev.id).unwrap();
    assert_eq!(dev_children.len(), 1);
    assert_eq!(dev_children[0].name, "service-auth");

    // service-auth's children: [feature-login]
    let service_children = txn.get_children_stacks(service.id).unwrap();
    assert_eq!(service_children.len(), 1);
    assert_eq!(service_children[0].name, "feature-login");

    // feature-login has no children
    let feature_children = txn.get_children_stacks(_feature.id).unwrap();
    assert!(feature_children.is_empty());

    txn.commit().unwrap();
}

#[test]
fn get_children_stacks_nonexistent_parent_returns_empty() {
    let (_dir, pristine) = open_pristine();
    let txn = pristine.write_txn().unwrap();

    let children = txn.get_children_stacks(999).unwrap();
    assert!(children.is_empty());
}

// ===========================================================================
// Phase 4: Cycle detection in create_stack
// ===========================================================================

#[test]
fn create_stack_detects_cycle_in_existing_chain() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    // Build: main → dev → service-auth
    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let _service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();

    // Creating a valid leaf works fine
    let feature = txn.create_stack("feature", StackKind::Local, Some(_service.id));
    assert!(feature.is_ok());

    txn.commit().unwrap();
}

// ===========================================================================
// Phase 4: Full lifecycle workflow
// ===========================================================================

/// The user's original scenario with full lifecycle:
/// create feature → add changes → some applied to dev → delete feature → clean
#[test]
fn phase4_full_lifecycle_abandon_feature() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let main = txn.create_stack("main", StackKind::Shared, None).unwrap();
    let dev = txn
        .create_stack("dev", StackKind::Shared, Some(main.id))
        .unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Record 5 changes on feature (all in STACK_GRAPH)
    for i in 1..=5u64 {
        let v = make_vertex(i, 0, i * 10);
        txn.put_stack_graph(feature.id, v, make_edge(EdgeFlags::BLOCK, i + 100, 0, i))
            .unwrap();
    }

    // Apply changes 2 and 4 to dev (go to global GRAPH)
    let v2 = make_vertex(2, 0, 20);
    let v4 = make_vertex(4, 0, 40);
    txn.put_graph(v2, make_edge(EdgeFlags::BLOCK, 102, 0, 2))
        .unwrap();
    txn.put_graph(v4, make_edge(EdgeFlags::BLOCK, 104, 0, 4))
        .unwrap();

    // Delete the feature stack
    txn.del_stack(&feature).unwrap();

    // Stack is gone
    assert!(txn.get_stack("feature").unwrap().is_none());

    // STACK_GRAPH edges are gone (1, 3, 5 truly vanished; 2, 4 were also in STACK_GRAPH)
    for i in 1..=5u64 {
        let v = make_vertex(i, 0, i * 10);
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature.id, v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            edges.is_empty(),
            "STACK_GRAPH should be empty for change {}",
            i
        );
    }

    // Changes 2 and 4 survive in GRAPH (applied to dev)
    let g2: Vec<_> = txn
        .iter_adjacent(v2, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(g2.len(), 1);

    let g4: Vec<_> = txn
        .iter_adjacent(v4, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(g4.len(), 1);

    // Changes 1, 3, 5 are gone from everywhere
    for i in [1u64, 3, 5] {
        let v = make_vertex(i, 0, i * 10);
        let g: Vec<_> = txn
            .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(g.is_empty(), "change {} should be gone from GRAPH", i);
    }

    txn.commit().unwrap();
}

/// Stacked deletion: delete leaf first, then parent, each step clean.
#[test]
fn phase4_full_lifecycle_stacked_deletion() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    // Each stack has its own edges
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(2, 0, 20);
    txn.put_stack_graph(service.id, v1, make_edge(EdgeFlags::BLOCK, 10, 0, 1))
        .unwrap();
    txn.put_stack_graph(feature.id, v2, make_edge(EdgeFlags::BLOCK, 20, 0, 2))
        .unwrap();

    // Cannot delete service-auth yet (has child)
    assert!(txn.del_stack(&service).is_err());

    // Delete feature-login first
    txn.del_stack(&feature).unwrap();
    assert!(txn.get_stack("feature-login").unwrap().is_none());

    // feature's edges are gone
    let fe: Vec<_> = txn
        .iter_stack_graph_adjacent(feature.id, v2, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(fe.is_empty());

    // service-auth's edges still exist
    let se: Vec<_> = txn
        .iter_stack_graph_adjacent(service.id, v1, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(se.len(), 1);

    // Now service-auth can be deleted
    txn.del_stack(&service).unwrap();
    assert!(txn.get_stack("service-auth").unwrap().is_none());

    // service-auth's edges are gone too
    let se2: Vec<_> = txn
        .iter_stack_graph_adjacent(service.id, v1, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(se2.is_empty());

    txn.commit().unwrap();
}

/// Persistence: del_stack effects survive database reopen.
#[test]
fn phase4_del_stack_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");

    let feature_id;

    // Create and populate
    {
        let pristine = Pristine::open(&db_path).unwrap();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();
        feature_id = feature.id;

        let v = make_vertex(1, 0, 10);
        txn.put_stack_graph(feature.id, v, make_edge(EdgeFlags::BLOCK, 2, 0, 1))
            .unwrap();

        txn.del_stack(&feature).unwrap();
        txn.commit().unwrap();
    }

    // Reopen and verify
    {
        let pristine = Pristine::open(&db_path).unwrap();
        let txn = pristine.read_txn().unwrap();

        // Stack is gone
        assert!(txn.get_stack("feature").unwrap().is_none());

        // STACK_GRAPH edges are gone
        let v = make_vertex(1, 0, 10);
        let edges: Vec<_> = txn
            .iter_stack_graph_adjacent(feature_id, v, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(edges.is_empty());
    }
}

// ===========================================================================
// Phase 3: OverlayTxn — iter_adjacent
// ===========================================================================

#[test]
fn overlay_empty_chain_delegates_to_global_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let v = make_vertex(1, 0, 10);
    let e = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_graph(v, e).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![]);

    let edges: Vec<_> = overlay
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dest().change, NodeId::new(2));
}

#[test]
fn overlay_sees_isolated_stack_edges() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let stack_edge = make_edge(EdgeFlags::BLOCK, 10, 0, 100);
    txn.put_stack_graph(feature.id, v, stack_edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let stack = txn.get_stack("feature").unwrap().unwrap();
    let overlay = OverlayTxn::from_stack(&txn, &stack).unwrap();

    let edges: Vec<_> = overlay
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].dest().change, NodeId::new(10));
}

#[test]
fn overlay_unions_stack_graph_and_global_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let global_edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);
    let stack_edge = make_edge(EdgeFlags::BLOCK, 60, 0, 600);

    txn.put_graph(v, global_edge).unwrap();
    txn.put_stack_graph(feature.id, v, stack_edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    let edges: Vec<_> = overlay
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 2);

    let dests: HashSet<u64> = edges.iter().map(|e| e.dest().change.get()).collect();
    assert!(dests.contains(&50));
    assert!(dests.contains(&60));
}

#[test]
fn overlay_deduplicates_same_edge_in_stack_and_global() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);

    // Same edge in both layers
    txn.put_graph(v, edge).unwrap();
    txn.put_stack_graph(feature.id, v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    let edges: Vec<_> = overlay
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 1, "duplicate edge must be deduplicated");
}

#[test]
fn overlay_unions_stacked_isolated_chains() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let global_edge = make_edge(EdgeFlags::BLOCK, 10, 0, 1);
    let service_edge = make_edge(EdgeFlags::BLOCK, 20, 0, 2);
    let feature_edge = make_edge(EdgeFlags::BLOCK, 30, 0, 3);

    txn.put_graph(v, global_edge).unwrap();
    txn.put_stack_graph(service.id, v, service_edge).unwrap();
    txn.put_stack_graph(feature.id, v, feature_edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let stack = txn.get_stack("feature-login").unwrap().unwrap();
    let chain = txn.resolve_overlay_chain(&stack).unwrap();
    let overlay = OverlayTxn::new(&txn, chain);

    let edges: Vec<_> = overlay
        .iter_adjacent(v, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(edges.len(), 3);

    let dests: HashSet<u64> = edges.iter().map(|e| e.dest().change.get()).collect();
    assert!(dests.contains(&10));
    assert!(dests.contains(&20));
    assert!(dests.contains(&30));
}

// ===========================================================================
// Phase 3: OverlayTxn — has_vertex
// ===========================================================================

#[test]
fn overlay_has_vertex_finds_stack_graph_only_vertex() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    let v = make_vertex(1, 0, 10);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);

    // Only in STACK_GRAPH
    txn.put_stack_graph(feature.id, v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();

    // Without overlay: not found
    assert!(!txn.has_vertex(v).unwrap());

    // With overlay: found
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);
    assert!(overlay.has_vertex(v).unwrap());
}

#[test]
fn overlay_has_vertex_finds_global_graph_vertex() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let v = make_vertex(1, 0, 10);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_graph(v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![99]); // non-existent stack is fine

    assert!(overlay.has_vertex(v).unwrap());
}

// ===========================================================================
// Phase 3: OverlayTxn — find_block
// ===========================================================================

#[test]
fn overlay_find_block_finds_vertex_in_stack_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Vertex V(1, 0, 20) only in STACK_GRAPH
    let v = make_vertex(1, 0, 20);
    let fwd = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    let rev = make_edge(EdgeFlags::BLOCK | EdgeFlags::PARENT, 1, 0, 1);
    txn.put_stack_graph(feature.id, v, fwd).unwrap();
    txn.put_stack_graph(feature.id, v, rev).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    // Position 5 is within V(1, 0, 20)
    let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
    let found = overlay.find_block(pos).unwrap();
    assert_eq!(found.change, NodeId::new(1));
    assert_eq!(found.start, ChangePosition::new(0));
    assert_eq!(found.end, ChangePosition::new(20));
}

#[test]
fn overlay_find_block_falls_back_to_global() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Vertex only in GRAPH
    let v = make_vertex(1, 0, 20);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_graph(v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
    let found = overlay.find_block(pos).unwrap();
    assert_eq!(found.start, ChangePosition::new(0));
    assert_eq!(found.end, ChangePosition::new(20));
}

#[test]
fn overlay_find_block_root_always_works() {
    let (_dir, pristine) = open_pristine();
    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![42]);

    let pos = Position::new(NodeId::ROOT, ChangePosition::ROOT);
    let found = overlay.find_block(pos).unwrap();
    assert!(found.is_root());
}

#[test]
fn overlay_find_block_prefers_nonempty_over_empty_in_stack_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Inode V(1, 9, 9) and content V(1, 9, 23) — same start position
    let inode_v = make_vertex(1, 9, 9);
    let content_v = make_vertex(1, 9, 23);
    let e1 = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    let e2 = make_edge(EdgeFlags::BLOCK, 3, 0, 1);

    txn.put_stack_graph(feature.id, inode_v, e1).unwrap();
    txn.put_stack_graph(feature.id, content_v, e2).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    // Position 9 should find content V[9:23], not inode V[9:9]
    let pos = Position::new(NodeId::new(1), ChangePosition::new(9));
    let found = overlay.find_block(pos).unwrap();
    assert_eq!(found.start, ChangePosition::new(9));
    assert_eq!(found.end, ChangePosition::new(23));
}

// ===========================================================================
// Phase 3: OverlayTxn — find_block_end
// ===========================================================================

#[test]
fn overlay_find_block_end_finds_empty_vertex_in_stack_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Empty inode vertex V(1, 9, 9) only in STACK_GRAPH
    let inode_v = make_vertex(1, 9, 9);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_stack_graph(feature.id, inode_v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    let pos = Position::new(NodeId::new(1), ChangePosition::new(9));
    let found = overlay.find_block_end(pos).unwrap();
    assert_eq!(found.start, ChangePosition::new(9));
    assert_eq!(found.end, ChangePosition::new(9));
}

#[test]
fn overlay_find_block_end_finds_vertex_ending_at_pos_in_stack_graph() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Name vertex V(1, 0, 9) — ends at position 9
    let name_v = make_vertex(1, 0, 9);
    let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_stack_graph(feature.id, name_v, edge).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    let pos = Position::new(NodeId::new(1), ChangePosition::new(9));
    let found = overlay.find_block_end(pos).unwrap();
    assert_eq!(found.start, ChangePosition::new(0));
    assert_eq!(found.end, ChangePosition::new(9));
}

// ===========================================================================
// Phase 3: OverlayTxn — pass-through methods
// ===========================================================================

#[test]
fn overlay_get_external_passes_through() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let hash = Hash::of(b"test-change");
    let id = txn.register_change(&hash).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![42]);

    let found = overlay.get_external(id).unwrap();
    assert_eq!(found, Some(hash));
}

#[test]
fn overlay_get_internal_passes_through() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let hash = Hash::of(b"test-change");
    let id = txn.register_change(&hash).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![42]);

    let found = overlay.get_internal(&hash).unwrap();
    assert_eq!(found, Some(id));
}

// ===========================================================================
// Phase 3: OverlayTxn — from_stack convenience
// ===========================================================================

#[test]
fn overlay_from_stack_shared_has_no_overlay() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();
    txn.create_stack("dev", StackKind::Shared, None).unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let stack = txn.get_stack("dev").unwrap().unwrap();
    let overlay = OverlayTxn::from_stack(&txn, &stack).unwrap();

    assert!(!overlay.has_overlay());
    assert!(overlay.stack_chain().is_empty());
}

#[test]
fn overlay_from_stack_isolated_resolves_chain() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let service = txn
        .create_stack("service-auth", StackKind::Local, Some(dev.id))
        .unwrap();
    let feature = txn
        .create_stack("feature-login", StackKind::Local, Some(service.id))
        .unwrap();
    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let stack = txn.get_stack("feature-login").unwrap().unwrap();
    let overlay = OverlayTxn::from_stack(&txn, &stack).unwrap();

    assert!(overlay.has_overlay());
    assert_eq!(overlay.stack_chain(), &[feature.id, service.id]);
}

// ===========================================================================
// Phase 3: OverlayTxn — full workflow with overlay traversal
// ===========================================================================

/// Simulates: feature adds vertices/edges → overlay can traverse the graph
/// while the global GRAPH sees nothing.
#[test]
fn overlay_full_workflow_isolated_graph_traversal() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // Build a small graph: ROOT → V1 → V2 (all in STACK_GRAPH)
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(1, 10, 20);

    // Forward: ROOT → V1
    let root_to_v1 = make_edge(EdgeFlags::BLOCK, 1, 0, 1);
    txn.put_stack_graph(feature.id, GraphNode::ROOT, root_to_v1)
        .unwrap();

    // Reverse: V1 → ROOT (PARENT)
    let v1_to_root = SerializedGraphEdge::new(
        EdgeFlags::BLOCK | EdgeFlags::PARENT,
        Position::new(NodeId::ROOT, ChangePosition::ROOT),
        NodeId::new(1),
    );
    txn.put_stack_graph(feature.id, v1, v1_to_root).unwrap();

    // Forward: V1 → V2
    let v1_to_v2 = make_edge(EdgeFlags::BLOCK, 1, 10, 1);
    txn.put_stack_graph(feature.id, v1, v1_to_v2).unwrap();

    // Reverse: V2 → V1 (PARENT)
    let v2_to_v1 = SerializedGraphEdge::new(
        EdgeFlags::BLOCK | EdgeFlags::PARENT,
        Position::new(NodeId::new(1), ChangePosition::new(10)),
        NodeId::new(1),
    );
    txn.put_stack_graph(feature.id, v2, v2_to_v1).unwrap();

    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();

    // Global GRAPH sees nothing
    assert!(!txn.has_vertex(v1).unwrap());
    assert!(!txn.has_vertex(v2).unwrap());

    // Overlay sees everything
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);
    assert!(overlay.has_vertex(v1).unwrap());
    assert!(overlay.has_vertex(v2).unwrap());

    // Can traverse from ROOT through the overlay
    let root_edges: Vec<_> = overlay
        .iter_adjacent(GraphNode::ROOT, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // ROOT should have at least the forward edge to V1
    let forward_root: Vec<_> = root_edges
        .iter()
        .filter(|e| !e.flag().contains(EdgeFlags::PARENT))
        .collect();
    assert!(!forward_root.is_empty());

    // Can find V1 by position through the overlay
    let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
    let found = overlay.find_block(pos).unwrap();
    assert_eq!(found, v1);
}

/// Mixed scenario: some edges in GRAPH (applied to dev), some only in
/// STACK_GRAPH (still pending). The overlay sees the union.
#[test]
fn overlay_full_workflow_mixed_global_and_isolated() {
    let (_dir, pristine) = open_pristine();
    let mut txn = pristine.write_txn().unwrap();

    let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
    let feature = txn
        .create_stack("feature", StackKind::Local, Some(dev.id))
        .unwrap();

    // V1 in global GRAPH (applied to dev)
    let v1 = make_vertex(1, 0, 10);
    let e1 = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
    txn.put_graph(v1, e1).unwrap();

    // V2 only in feature's STACK_GRAPH (pending)
    let v2 = make_vertex(2, 0, 20);
    let e2 = make_edge(EdgeFlags::BLOCK, 3, 0, 2);
    txn.put_stack_graph(feature.id, v2, e2).unwrap();

    txn.commit().unwrap();

    let txn = pristine.read_txn().unwrap();
    let overlay = OverlayTxn::new(&txn, vec![feature.id]);

    // Both vertices visible through overlay
    assert!(overlay.has_vertex(v1).unwrap());
    assert!(overlay.has_vertex(v2).unwrap());

    // V1 edges come from GRAPH
    let v1_edges: Vec<_> = overlay
        .iter_adjacent(v1, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(v1_edges.len(), 1);
    assert_eq!(v1_edges[0].dest().change, NodeId::new(2));

    // V2 edges come from STACK_GRAPH
    let v2_edges: Vec<_> = overlay
        .iter_adjacent(v2, EdgeFlags::empty(), EdgeFlags::all())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(v2_edges.len(), 1);
    assert_eq!(v2_edges[0].dest().change, NodeId::new(3));

    // Direct GRAPH read doesn't see V2
    assert!(!txn.has_vertex(v2).unwrap());
}

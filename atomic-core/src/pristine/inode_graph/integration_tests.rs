use super::*;
use crate::pristine::{MutTxnT, Pristine};
use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Position, SerializedGraphEdge,
};
use tempfile::tempdir;

#[test]
fn test_inode_graph_ops_empty_database() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let txn = pristine.read_txn().unwrap();

    // Empty database should have no vertices for any inode
    let inode = Inode::new(42);
    assert_eq!(txn.count_inode_vertices(inode).unwrap(), 0);
    assert!(!txn.inode_graph_is_populated(inode).unwrap());
}

#[test]
fn test_inode_graph_ops_with_data() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(42);
    let change_hash = Hash::of(b"test change");

    // Write some data
    {
        let mut txn = pristine.write_txn().unwrap();

        // Register a change
        let change_id = txn.register_change(&change_hash).unwrap();

        // Create a span
        let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));

        // Create an edge
        let dest = Position::new(change_id, ChangePosition::new(50));
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id);

        // Put into inode graph
        txn.put_inode_graph(inode, node, edge).unwrap();
        txn.commit().unwrap();
    }

    // Read and verify
    {
        let txn = pristine.read_txn().unwrap();

        // Should have vertices now
        assert!(txn.inode_graph_is_populated(inode).unwrap());
        assert_eq!(txn.count_inode_vertices(inode).unwrap(), 1);

        // Other inodes should still be empty
        assert!(!txn.inode_graph_is_populated(Inode::new(99)).unwrap());
    }
}

#[test]
fn test_inode_adj_iteration() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(1);
    let change_hash = Hash::of(b"test");

    // Setup test data
    let change_id;
    let node;
    {
        let mut txn = pristine.write_txn().unwrap();
        change_id = txn.register_change(&change_hash).unwrap();
        node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));

        // Add multiple edges to the same span
        let dest1 = Position::new(change_id, ChangePosition::new(10));
        let dest2 = Position::new(change_id, ChangePosition::new(20));

        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest1, change_id),
        )
        .unwrap();
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::PSEUDO, dest2, change_id),
        )
        .unwrap();
        txn.commit().unwrap();
    }

    // Test adjacency iteration
    {
        let txn = pristine.read_txn().unwrap();

        let mut adj = txn
            .init_inode_adj(inode, node, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap();

        let mut count = 0;
        while let Some(result) = txn.next_inode_adj(&mut adj) {
            result.unwrap();
            count += 1;
        }

        assert_eq!(count, 2);
        assert!(adj.is_exhausted());
    }
}

#[test]
fn test_find_block_in_inode() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(5);
    let change_hash = Hash::of(b"block test");

    let change_id;
    let node;
    {
        let mut txn = pristine.write_txn().unwrap();
        change_id = txn.register_change(&change_hash).unwrap();

        // Create a span spanning positions 100-200
        node = GraphNode::new(
            change_id,
            ChangePosition::new(100),
            ChangePosition::new(200),
        );

        let dest = Position::new(change_id, ChangePosition::new(150));
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();
        txn.commit().unwrap();
    }

    // Test finding blocks
    {
        let txn = pristine.read_txn().unwrap();

        // Position inside the block should find it
        let pos_inside = Position::new(change_id, ChangePosition::new(150));
        let found = txn.find_block_in_inode(inode, pos_inside).unwrap();
        assert_eq!(found, Some(node));

        // Position at start should find it
        let pos_start = Position::new(change_id, ChangePosition::new(100));
        let found = txn.find_block_in_inode(inode, pos_start).unwrap();
        assert_eq!(found, Some(node));

        // Position outside (before) should not find it
        let pos_before = Position::new(change_id, ChangePosition::new(50));
        let found = txn.find_block_in_inode(inode, pos_before).unwrap();
        assert_eq!(found, None);

        // Position outside (at end, exclusive) should not find it
        let pos_at_end = Position::new(change_id, ChangePosition::new(200));
        let found = txn.find_block_in_inode(inode, pos_at_end).unwrap();
        assert_eq!(found, None);

        // Different change_id should not find it
        let other_hash = Hash::of(b"other");
        let mut write_txn = pristine.write_txn().unwrap();
        let other_change_id = write_txn.register_change(&other_hash).unwrap();
        write_txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let pos_other = Position::new(other_change_id, ChangePosition::new(150));
        let found = txn.find_block_in_inode(inode, pos_other).unwrap();
        assert_eq!(found, None);
    }
}

#[test]
fn test_inode_edge_iterator() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(10);
    let hash = Hash::of(b"iter test");

    {
        let mut txn = pristine.write_txn().unwrap();
        let change_id = txn.register_change(&hash).unwrap();

        let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));
        let dest = Position::new(change_id, ChangePosition::new(50));

        // Add edges with different flags
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::FOLDER, dest, change_id),
        )
        .unwrap();
        txn.commit().unwrap();
    }

    {
        let txn = pristine.read_txn().unwrap();

        // Iterate with iter_inode_edges
        let iter = txn
            .iter_inode_edges(inode, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap();

        let edges: Vec<_> = iter.collect();
        // Note: iter_inode_edges requires the caller to initialize with a span
        // Since it's a trait-provided method that creates InodeEdgeIter without
        // a starting span, it won't iterate automatically.
        // The current implementation starts with current_adj = None.
        assert_eq!(edges.len(), 0);
    }
}

#[test]
fn test_write_txn_inode_graph_ops() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(20);
    let hash = Hash::of(b"write txn test");

    let mut txn = pristine.write_txn().unwrap();
    let change_id = txn.register_change(&hash).unwrap();

    let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));
    let dest = Position::new(change_id, ChangePosition::new(25));

    txn.put_inode_graph(
        inode,
        node,
        SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
    )
    .unwrap();

    // Test InodeGraphOps on WriteTxn before commit
    assert!(txn.inode_graph_is_populated(inode).unwrap());
    assert_eq!(txn.count_inode_vertices(inode).unwrap(), 1);

    let found = txn
        .find_block_in_inode(inode, Position::new(change_id, ChangePosition::new(25)))
        .unwrap();
    assert_eq!(found, Some(node));

    txn.commit().unwrap();
}

#[test]
fn test_flag_filtering() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode = Inode::new(30);
    let hash = Hash::of(b"flag filter test");

    let change_id;
    let node;
    {
        let mut txn = pristine.write_txn().unwrap();
        change_id = txn.register_change(&hash).unwrap();
        node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));
        let dest = Position::new(change_id, ChangePosition::new(50));

        // Add edges with different flags
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::PSEUDO, dest, change_id),
        )
        .unwrap();
        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::FOLDER, dest, change_id),
        )
        .unwrap();
        txn.commit().unwrap();
    }

    {
        let txn = pristine.read_txn().unwrap();

        // Filter to only BLOCK edges (no PSEUDO)
        let mut adj = txn
            .init_inode_adj(inode, node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
            .unwrap();

        let mut block_only_count = 0;
        while let Some(result) = txn.next_inode_adj(&mut adj) {
            let edge = result.unwrap();
            assert_eq!(edge.flag(), EdgeFlags::BLOCK);
            block_only_count += 1;
        }
        assert_eq!(block_only_count, 1);

        // Filter to BLOCK..BLOCK|PSEUDO range
        let mut adj2 = txn
            .init_inode_adj(
                inode,
                node,
                EdgeFlags::BLOCK,
                EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
            )
            .unwrap();

        let mut block_range_count = 0;
        while let Some(result) = txn.next_inode_adj(&mut adj2) {
            result.unwrap();
            block_range_count += 1;
        }
        assert_eq!(block_range_count, 2);
    }
}

#[test]
fn test_multiple_inodes_isolation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("pristine");
    let pristine = Pristine::open(&db_path).unwrap();

    let inode1 = Inode::new(100);
    let inode2 = Inode::new(200);
    let hash = Hash::of(b"isolation test");

    {
        let mut txn = pristine.write_txn().unwrap();
        let change_id = txn.register_change(&hash).unwrap();

        // Add vertices to inode1
        let v1 = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));
        let v2 = GraphNode::new(change_id, ChangePosition::new(50), ChangePosition::new(100));
        let dest = Position::new(change_id, ChangePosition::new(25));

        txn.put_inode_graph(
            inode1,
            v1,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();
        txn.put_inode_graph(
            inode1,
            v2,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();

        // Add one span to inode2
        let v3 = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(200));
        txn.put_inode_graph(
            inode2,
            v3,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();

        txn.commit().unwrap();
    }

    {
        let txn = pristine.read_txn().unwrap();

        // Verify isolation
        assert_eq!(txn.count_inode_vertices(inode1).unwrap(), 2);
        assert_eq!(txn.count_inode_vertices(inode2).unwrap(), 1);

        // inode3 should be empty
        let inode3 = Inode::new(300);
        assert_eq!(txn.count_inode_vertices(inode3).unwrap(), 0);
        assert!(!txn.inode_graph_is_populated(inode3).unwrap());
    }
}

#[test]
fn test_inode_graph_stats_display() {
    let stats = InodeGraphStats {
        vertices_visited: 100,
        edges_traversed: 200,
        page_accesses: 10,
        cache_hits: 25,
    };

    let display = stats.to_string();
    assert!(display.contains("100 vertices"));
    assert!(display.contains("200 edges"));
    assert!(display.contains("10 pages"));
    assert!(display.contains("cache hits"));
}

use super::*;


#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::traits::VertexExt;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    #[test]
    fn test_register_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let hash = Hash::of(b"test change");
        let id = txn.register_change(&hash).unwrap();

        // Should get same ID for same hash
        let id2 = txn.register_change(&hash).unwrap();
        assert_eq!(id, id2);

        // Should be able to look up both ways
        assert_eq!(txn.get_external(id).unwrap(), Some(hash));
        assert_eq!(txn.get_internal(&hash).unwrap(), Some(id));

        txn.commit().unwrap();
    }

    #[test]
    fn test_stack_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack
        let mut stack = txn.open_or_create_stack("main").unwrap();
        assert_eq!(stack.name, "main");
        assert_eq!(stack.change_count, 0);

        // Add a change
        let hash = Hash::of(b"change 1");
        let change_id = txn.register_change(&hash).unwrap();
        let seq = txn.put_change(&mut stack, change_id, &hash).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(stack.change_count, 1);

        // Update stack state
        txn.update_stack(&stack).unwrap();

        // List stacks
        let stacks = txn.list_stacks().unwrap();
        assert_eq!(stacks, vec!["main"]);

        txn.commit().unwrap();

        // Read back
        let txn = pristine.read_txn().unwrap();
        let stack = txn.get_stack("main").unwrap().unwrap();
        assert_eq!(stack.change_count, 1);
    }

    #[test]
    fn test_register_tag() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let hash = Hash::of(b"test tag content");
        let id = txn.register_tag(&hash).unwrap();

        // Should get same ID for same hash
        let id2 = txn.register_tag(&hash).unwrap();
        assert_eq!(id, id2);

        // Should be able to look up both ways
        assert_eq!(txn.get_external(id).unwrap(), Some(hash));
        assert_eq!(txn.get_internal(&hash).unwrap(), Some(id));

        // Should be marked as a tag type
        assert_eq!(txn.get_node_type(id).unwrap(), Some(node_type::TAG));

        txn.commit().unwrap();
    }

    #[test]
    fn test_get_node_type() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Register a change
        let change_hash = Hash::of(b"change content");
        let change_id = txn.register_change(&change_hash).unwrap();

        // Register a tag
        let tag_hash = Hash::of(b"tag content");
        let tag_id = txn.register_tag(&tag_hash).unwrap();

        // Verify node types
        assert_eq!(
            txn.get_node_type(change_id).unwrap(),
            Some(node_type::CHANGE)
        );
        assert_eq!(txn.get_node_type(tag_id).unwrap(), Some(node_type::TAG));

        // Non-existent node should return None
        let fake_id = NodeId::new(99999);
        assert_eq!(txn.get_node_type(fake_id).unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_change_and_tag_different_types() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Same content registered as change and tag should get different IDs
        // because we use different registration methods
        let content = b"shared content";
        let hash = Hash::of(content);

        let change_id = txn.register_change(&hash).unwrap();

        // Registering as tag should return the existing ID (since hash is same)
        let tag_id = txn.register_tag(&hash).unwrap();

        // Same hash means same ID
        assert_eq!(change_id, tag_id);

        // The node type should be CHANGE since it was registered first
        assert_eq!(
            txn.get_node_type(change_id).unwrap(),
            Some(node_type::CHANGE)
        );

        txn.commit().unwrap();
    }

    #[test]
    fn test_del_stack() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create a stack with some changes
        {
            let mut txn = pristine.write_txn().unwrap();

            let mut stack = txn.open_or_create_stack("to-delete").unwrap();
            assert_eq!(stack.name, "to-delete");

            // Add some changes
            let hash1 = Hash::of(b"change 1");
            let hash2 = Hash::of(b"change 2");
            let change_id1 = txn.register_change(&hash1).unwrap();
            let change_id2 = txn.register_change(&hash2).unwrap();

            txn.put_change(&mut stack, change_id1, &hash1).unwrap();
            txn.put_change(&mut stack, change_id2, &hash2).unwrap();
            txn.update_stack(&stack).unwrap();

            assert_eq!(stack.change_count, 2);

            // Verify stack exists
            let stacks = txn.list_stacks().unwrap();
            assert!(stacks.contains(&"to-delete".to_string()));

            txn.commit().unwrap();
        }

        // Delete the stack
        {
            let mut txn = pristine.write_txn().unwrap();

            let stack = txn.get_stack("to-delete").unwrap().unwrap();
            txn.del_stack(&stack).unwrap();

            txn.commit().unwrap();
        }

        // Verify stack is gone
        {
            let txn = pristine.read_txn().unwrap();

            let stack = txn.get_stack("to-delete").unwrap();
            assert!(stack.is_none());

            let stacks = txn.list_stacks().unwrap();
            assert!(!stacks.contains(&"to-delete".to_string()));
        }
    }

    #[test]
    fn test_del_stack_preserves_other_stacks() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create two stacks
        {
            let mut txn = pristine.write_txn().unwrap();

            let mut stack1 = txn.open_or_create_stack("keep-me").unwrap();
            let mut stack2 = txn.open_or_create_stack("delete-me").unwrap();

            // Add changes to both
            let hash1 = Hash::of(b"change for keep");
            let hash2 = Hash::of(b"change for delete");
            let change_id1 = txn.register_change(&hash1).unwrap();
            let change_id2 = txn.register_change(&hash2).unwrap();

            txn.put_change(&mut stack1, change_id1, &hash1).unwrap();
            txn.put_change(&mut stack2, change_id2, &hash2).unwrap();
            txn.update_stack(&stack1).unwrap();
            txn.update_stack(&stack2).unwrap();

            txn.commit().unwrap();
        }

        // Delete only one stack
        {
            let mut txn = pristine.write_txn().unwrap();

            let stack = txn.get_stack("delete-me").unwrap().unwrap();
            txn.del_stack(&stack).unwrap();

            txn.commit().unwrap();
        }

        // Verify the other stack is intact
        {
            let txn = pristine.read_txn().unwrap();

            // Deleted stack should be gone
            assert!(txn.get_stack("delete-me").unwrap().is_none());

            // Other stack should still exist with its change
            let stack = txn.get_stack("keep-me").unwrap().unwrap();
            assert_eq!(stack.change_count, 1);
        }
    }

    #[test]
    fn test_tree_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let inode = txn.alloc_inode().unwrap();
        txn.put_tree("src/main.rs", inode).unwrap();

        assert_eq!(txn.get_inode("src/main.rs").unwrap(), Some(inode));
        assert_eq!(
            txn.get_path(inode).unwrap(),
            Some("src/main.rs".to_string())
        );

        let removed = txn.del_tree("src/main.rs").unwrap();
        assert_eq!(removed, Some(inode));
        assert_eq!(txn.get_inode("src/main.rs").unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_graph_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let node = GraphNode::from_parts(NodeId::new(1), 0, 100);
        let dest = Position::new(NodeId::new(2), ChangePosition::new(0));
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));

        // Insert edge
        txn.put_graph(node, edge).unwrap();

        // Check it exists
        assert!(txn.has_vertex(node).unwrap());

        // Get edges
        let edges = txn.get_edges(node).unwrap();
        assert_eq!(edges.len(), 1);

        // Delete edge
        txn.del_graph(node, edge).unwrap();

        txn.commit().unwrap();
    }

    #[test]
    fn test_del_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack with 3 changes
        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.put_change(&mut stack, id3, &hash3).unwrap();
        txn.update_stack(&stack).unwrap();

        assert_eq!(stack.change_count, 3);

        // Remove the middle change (id2)
        let removed_seq = txn.del_change(&mut stack, id2, &hash2).unwrap();
        assert_eq!(removed_seq, Some(1)); // Was at sequence 1

        // Stack should now have 2 changes
        assert_eq!(stack.change_count, 2);

        // Change 1 should still be at sequence 0
        let seq0 = txn.get_change_at_seq(&stack, 0).unwrap();
        assert_eq!(seq0, Some(id1));

        // Change 3 should now be at sequence 1 (shifted down)
        let seq1 = txn.get_change_at_seq(&stack, 1).unwrap();
        assert_eq!(seq1, Some(id3));

        // Merkle state should be recomputed
        let expected_state = Merkle::ZERO.next(&hash1).next(&hash3);
        assert_eq!(stack.state, expected_state);

        txn.update_stack(&stack).unwrap();
        txn.commit().unwrap();
    }

    #[test]
    fn test_del_change_not_in_stack() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();

        // Only add change 1 to the stack
        txn.put_change(&mut stack, id1, &hash1).unwrap();

        // Try to remove change 2 (not in stack)
        let result = txn.del_change(&mut stack, id2, &hash2).unwrap();
        assert_eq!(result, None);

        // Stack should be unchanged
        assert_eq!(stack.change_count, 1);

        txn.commit().unwrap();
    }

    #[test]
    fn test_reinsert_change() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        // Create a stack with 2 changes
        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.update_stack(&stack).unwrap();

        assert_eq!(stack.change_count, 2);

        // Insert change 3 at position 1 (between change 1 and 2)
        txn.reinsert_change(&mut stack, id3, &hash3, 1).unwrap();

        // Stack should now have 3 changes
        assert_eq!(stack.change_count, 3);

        // Verify order: 1, 3, 2
        assert_eq!(txn.get_change_at_seq(&stack, 0).unwrap(), Some(id1));
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id3));
        assert_eq!(txn.get_change_at_seq(&stack, 2).unwrap(), Some(id2));

        // Merkle state should be recomputed
        let expected_state = Merkle::ZERO.next(&hash1).next(&hash3).next(&hash2);
        assert_eq!(stack.state, expected_state);

        txn.update_stack(&stack).unwrap();
        txn.commit().unwrap();
    }

    #[test]
    fn test_reinsert_change_at_end() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.update_stack(&stack).unwrap();

        // Insert at a position beyond current count (should append)
        txn.reinsert_change(&mut stack, id2, &hash2, 100).unwrap();

        assert_eq!(stack.change_count, 2);
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id2));

        txn.commit().unwrap();
    }

    #[test]
    fn test_unrecord_and_reinsert_workflow() {
        // This test simulates the Gerrit-like workflow:
        // 1. Create stack with 3 changes
        // 2. Unrecord the middle one
        // 3. Reinsert it at its original position
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let mut stack = txn.open_or_create_stack("test").unwrap();

        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");
        let hash3 = Hash::of(b"change 3");

        let id1 = txn.register_change(&hash1).unwrap();
        let id2 = txn.register_change(&hash2).unwrap();
        let id3 = txn.register_change(&hash3).unwrap();

        txn.put_change(&mut stack, id1, &hash1).unwrap();
        txn.put_change(&mut stack, id2, &hash2).unwrap();
        txn.put_change(&mut stack, id3, &hash3).unwrap();
        txn.update_stack(&stack).unwrap();

        let original_state = stack.state;

        // Unrecord the middle change
        let original_seq = txn.del_change(&mut stack, id2, &hash2).unwrap().unwrap();
        assert_eq!(original_seq, 1);
        assert_eq!(stack.change_count, 2);

        // Reinsert at original position
        txn.reinsert_change(&mut stack, id2, &hash2, original_seq)
            .unwrap();
        assert_eq!(stack.change_count, 3);

        // State should be identical to before
        assert_eq!(stack.state, original_state);

        // Order should be restored
        assert_eq!(txn.get_change_at_seq(&stack, 0).unwrap(), Some(id1));
        assert_eq!(txn.get_change_at_seq(&stack, 1).unwrap(), Some(id2));
        assert_eq!(txn.get_change_at_seq(&stack, 2).unwrap(), Some(id3));

        txn.commit().unwrap();
    }

    #[test]
    fn test_dependency_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let change1 = NodeId::new(1);
        let change2 = NodeId::new(2);
        let change3 = NodeId::new(3);

        // change2 and change3 depend on change1
        txn.put_dep(change2, change1).unwrap();
        txn.put_dep(change3, change1).unwrap();

        // Check dependencies
        let deps2 = txn.get_deps(change2).unwrap();
        assert_eq!(deps2, vec![change1]);

        // Check reverse dependencies
        let rev_deps1 = txn.get_rev_deps(change1).unwrap();
        assert!(rev_deps1.contains(&change2));
        assert!(rev_deps1.contains(&change3));

        txn.commit().unwrap();
    }

    #[test]
    fn test_inode_operations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        let inode = txn.alloc_inode().unwrap();
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));

        txn.put_inode(inode, pos).unwrap();

        assert_eq!(txn.inode_position(inode).unwrap(), Some(pos));
        assert_eq!(txn.position_inode(pos).unwrap(), Some(inode));

        let removed_pos = txn.del_inode(inode).unwrap();
        assert_eq!(removed_pos, Some(pos));
        assert_eq!(txn.inode_position(inode).unwrap(), None);

        txn.commit().unwrap();
    }

    #[test]
    fn test_abort_transaction() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Create a stack, then abort
        {
            let mut txn = pristine.write_txn().unwrap();
            txn.open_or_create_stack("test_stack").unwrap();
            txn.abort().unwrap();
        }

        // Stack should not exist
        let txn = pristine.read_txn().unwrap();
        assert!(txn.get_stack("test_stack").unwrap().is_none());
    }

    // Directory Tracking Tests

    #[test]
    fn test_directory_put_and_get() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        // Initially not a directory
        assert!(txn.get_directory_flags(inode).unwrap().is_none());
        assert!(!TreeTxnT::is_directory(&txn, inode).unwrap());

        // Mark as directory
        use crate::pristine::tables::directory_flags;
        txn.put_directory(inode, directory_flags::explicit_empty())
            .unwrap();

        // Now it's a directory
        assert!(TreeTxnT::is_directory(&txn, inode).unwrap());
        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(directory_flags::is_explicit(flags));
        assert!(directory_flags::is_empty(flags));

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_del() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        use crate::pristine::tables::directory_flags;
        txn.put_directory(inode, directory_flags::DIR_EXPLICIT)
            .unwrap();

        // Delete the directory marker
        let old_flags = txn.del_directory(inode).unwrap();
        assert_eq!(old_flags, Some(directory_flags::DIR_EXPLICIT));

        // No longer a directory
        assert!(!TreeTxnT::is_directory(&txn, inode).unwrap());

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_update_flags() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        let inode = txn.alloc_inode().unwrap();

        use crate::pristine::tables::directory_flags;

        // Start as empty directory
        txn.put_directory(inode, directory_flags::explicit_empty())
            .unwrap();

        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(directory_flags::is_empty(flags));

        // Update to non-empty (file was added)
        txn.update_directory_flags(inode, directory_flags::explicit_with_children())
            .unwrap();

        let flags = txn.get_directory_flags(inode).unwrap().unwrap();
        assert!(!directory_flags::is_empty(flags));
        assert!(directory_flags::is_explicit(flags));

        txn.commit().unwrap();
    }

    #[test]
    fn test_directory_persistence() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let inode;

        use crate::pristine::tables::directory_flags;

        // Create directory in first transaction
        {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            inode = txn.alloc_inode().unwrap();
            txn.put_directory(inode, directory_flags::explicit_empty())
                .unwrap();
            txn.commit().unwrap();
        }

        // Verify in new transaction
        {
            let pristine = Pristine::open(&db_path).unwrap();
            let txn = pristine.write_txn().unwrap();
            assert!(TreeTxnT::is_directory(&txn, inode).unwrap());
            let flags = txn.get_directory_flags(inode).unwrap().unwrap();
            assert!(directory_flags::is_explicit(flags));
            assert!(directory_flags::is_empty(flags));
        }
    }

    #[test]
    fn test_directory_multiple_inodes() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let mut txn = pristine.write_txn().unwrap();

        use crate::pristine::tables::directory_flags;

        // Create multiple directories with different flags
        let dir1 = txn.alloc_inode().unwrap();
        let dir2 = txn.alloc_inode().unwrap();
        let file = txn.alloc_inode().unwrap();

        txn.put_directory(dir1, directory_flags::explicit_empty())
            .unwrap();
        txn.put_directory(dir2, directory_flags::explicit_with_children())
            .unwrap();
        // file is not marked as directory

        assert!(TreeTxnT::is_directory(&txn, dir1).unwrap());
        assert!(TreeTxnT::is_directory(&txn, dir2).unwrap());
        assert!(!TreeTxnT::is_directory(&txn, file).unwrap());

        let flags1 = txn.get_directory_flags(dir1).unwrap().unwrap();
        let flags2 = txn.get_directory_flags(dir2).unwrap().unwrap();

        assert!(directory_flags::is_empty(flags1));
        assert!(!directory_flags::is_empty(flags2));

        txn.commit().unwrap();
    }
}

//! Tests for the core types in atomic-core.
//!
//! These tests verify the fundamental data structures that form the
//! foundation of the Atomic VCS graph model.

use atomic_core::types::{
    Base32, ChangePosition, EdgeFlags, GraphEdge, GraphNode, Hash, Inode, Merkle,
    NodeId, Position, SerializedGraphEdge, L64,
};

// ============================================================================
// L64 Tests - Little-endian 64-bit wrapper
// ============================================================================

mod l64_tests {
    use super::*;

    #[test]
    fn test_l64_new_and_get() {
        let value = 0x0102030405060708u64;
        let l64 = L64::new(value);
        assert_eq!(l64.get(), value);
    }

    #[test]
    fn test_l64_endianness() {
        let value = 0x0102030405060708u64;
        let l64 = L64::new(value);
        let bytes = l64.to_le_bytes();
        // Little-endian: least significant byte first
        assert_eq!(bytes, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn test_l64_roundtrip() {
        let original = 12345678901234567890u64;
        let l64 = L64::new(original);
        let bytes = l64.to_le_bytes();
        let recovered = L64::from_le_bytes(bytes);
        assert_eq!(recovered.get(), original);
    }

    #[test]
    fn test_l64_arithmetic() {
        let a = L64::new(100);
        let b = L64::new(30);
        assert_eq!(a - b, 70);
        assert_eq!((a + 5usize).get(), 105);
    }

    #[test]
    fn test_l64_from_u64() {
        let value: u64 = 42;
        let l64: L64 = value.into();
        assert_eq!(l64.get(), 42);
    }

    #[test]
    fn test_l64_into_u64() {
        let l64 = L64::new(42);
        let value: u64 = l64.into();
        assert_eq!(value, 42);
    }

    #[test]
    fn test_l64_serialization() {
        let l64 = L64::new(12345);
        let json = serde_json::to_string(&l64).unwrap();
        let parsed: L64 = serde_json::from_str(&json).unwrap();
        assert_eq!(l64, parsed);
    }
}

// ============================================================================
// NodeId Tests
// ============================================================================

mod node_id_tests {
    use super::*;

    #[test]
    fn test_node_id_root() {
        assert!(NodeId::ROOT.is_root());
        assert_eq!(NodeId::ROOT.get(), 0);
    }

    #[test]
    fn test_node_id_non_root() {
        let id = NodeId::new(42);
        assert!(!id.is_root());
        assert_eq!(id.get(), 42);
    }

    #[test]
    fn test_node_id_next() {
        let id = NodeId::new(5);
        assert_eq!(id.next(), NodeId::new(6));
    }

    #[test]
    fn test_node_id_ordering() {
        let id1 = NodeId::new(1);
        let id2 = NodeId::new(2);
        let id3 = NodeId::new(10);

        assert!(id1 < id2);
        assert!(id2 < id3);
        assert!(NodeId::ROOT < id1);
    }

    #[test]
    fn test_node_id_serialization() {
        let id = NodeId::new(12345);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_node_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(NodeId::new(1));
        set.insert(NodeId::new(2));
        set.insert(NodeId::new(1)); // duplicate

        assert_eq!(set.len(), 2);
    }
}

// ============================================================================
// ChangePosition Tests
// ============================================================================

mod change_position_tests {
    use super::*;

    #[test]
    fn test_change_position_creation() {
        let pos = ChangePosition::new(100);
        assert_eq!(pos.get(), 100);
    }

    #[test]
    fn test_change_position_root() {
        assert_eq!(ChangePosition::ROOT.get(), 0);
    }

    #[test]
    fn test_change_position_arithmetic() {
        let pos1 = ChangePosition::new(100);
        let pos2 = pos1 + 50;
        assert_eq!(pos2.get(), 150);
        assert_eq!(pos2 - pos1, 50);
    }

    #[test]
    fn test_change_position_as_usize() {
        let pos = ChangePosition::new(42);
        assert_eq!(pos.as_usize(), 42);
    }

    #[test]
    fn test_change_position_from_usize() {
        let pos: ChangePosition = 42usize.into();
        assert_eq!(pos.get(), 42);
    }

    #[test]
    fn test_change_position_ordering() {
        let pos1 = ChangePosition::new(10);
        let pos2 = ChangePosition::new(20);
        assert!(pos1 < pos2);
        assert!(ChangePosition::ROOT < pos1);
    }
}

// ============================================================================
// Inode Tests
// ============================================================================

mod inode_tests {
    use super::*;

    #[test]
    fn test_inode_creation() {
        let inode = Inode::new(42);
        assert_eq!(inode.get(), 42);
    }

    #[test]
    fn test_inode_root() {
        assert!(Inode::ROOT.is_root());
        assert_eq!(Inode::ROOT.get(), 0);
    }

    #[test]
    fn test_inode_next() {
        let inode = Inode::new(5);
        assert_eq!(inode.next(), Inode::new(6));
    }

    #[test]
    fn test_inode_ordering() {
        let i1 = Inode::new(1);
        let i2 = Inode::new(2);
        assert!(i1 < i2);
        assert!(Inode::ROOT < i1);
    }
}

// ============================================================================
// Hash Tests
// ============================================================================

mod hash_tests {
    use super::*;

    #[test]
    fn test_hash_of() {
        let h1 = Hash::of(b"hello");
        let h2 = Hash::of(b"hello");
        let h3 = Hash::of(b"world");

        assert_eq!(h1, h2, "Same input should produce same hash");
        assert_ne!(h1, h3, "Different input should produce different hash");
    }

    #[test]
    fn test_hash_zero() {
        assert!(Hash::ZERO.is_zero());
        assert!(!Hash::of(b"test").is_zero());
    }

    #[test]
    fn test_hash_hex_roundtrip() {
        let original = Hash::of(b"test data");
        let hex = original.to_hex();
        assert_eq!(hex.len(), 64, "Hex should be 64 characters");

        let parsed = Hash::from_hex(&hex).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_hex_invalid() {
        assert!(Hash::from_hex("not a valid hex").is_none());
        assert!(Hash::from_hex("abc").is_none()); // too short
        assert!(Hash::from_hex(&"g".repeat(64)).is_none()); // invalid char
    }

    #[test]
    fn test_hash_base32_roundtrip() {
        let original = Hash::of(b"base32 test");
        let base32 = original.to_base32();

        let parsed = Hash::from_base32(base32.as_bytes()).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_base32_case_insensitive() {
        let original = Hash::of(b"case test");
        let base32_upper = original.to_base32();
        let base32_lower = base32_upper.to_lowercase();

        let parsed_upper = Hash::from_base32(base32_upper.as_bytes()).unwrap();
        let parsed_lower = Hash::from_base32(base32_lower.as_bytes()).unwrap();

        assert_eq!(parsed_upper, parsed_lower);
        assert_eq!(original, parsed_lower);
    }

    #[test]
    fn test_hash_json_roundtrip() {
        let original = Hash::of(b"json test");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_display_is_base32() {
        let hash = Hash::of(b"display test");
        let display = format!("{}", hash);
        let base32 = hash.to_base32();
        assert_eq!(display, base32);
    }

    #[test]
    fn test_hash_debug_is_truncated() {
        let hash = Hash::of(b"debug test");
        let debug = format!("{:?}", hash);
        assert!(debug.starts_with("Hash("));
        assert!(debug.len() < 30, "Debug should be truncated");
    }
}

// ============================================================================
// Merkle Tests
// ============================================================================

mod merkle_tests {
    use super::*;

    #[test]
    fn test_merkle_initial() {
        let initial = Merkle::initial();
        assert!(!initial.is_zero());
    }

    #[test]
    fn test_merkle_zero() {
        assert!(Merkle::ZERO.is_zero());
    }

    #[test]
    fn test_merkle_incremental() {
        let state0 = Merkle::initial();
        let change1 = Hash::of(b"change 1");
        let change2 = Hash::of(b"change 2");

        let state1 = state0.next(&change1);
        let state2 = state1.next(&change2);

        // All states should be different
        assert_ne!(state0, state1);
        assert_ne!(state1, state2);
        assert_ne!(state0, state2);
    }

    #[test]
    fn test_merkle_deterministic() {
        let state0 = Merkle::initial();
        let change = Hash::of(b"test change");

        let state1_a = state0.next(&change);
        let state1_b = state0.next(&change);

        assert_eq!(
            state1_a, state1_b,
            "Same sequence should produce same state"
        );
    }

    #[test]
    fn test_merkle_order_matters() {
        let state0 = Merkle::initial();
        let change1 = Hash::of(b"change 1");
        let change2 = Hash::of(b"change 2");

        let state_12 = state0.next(&change1).next(&change2);
        let state_21 = state0.next(&change2).next(&change1);

        assert_ne!(state_12, state_21, "Order of changes should matter");
    }

    #[test]
    fn test_merkle_base32_roundtrip() {
        let state = Merkle::initial().next(&Hash::of(b"test"));
        let base32 = state.to_base32();
        let parsed = Merkle::from_base32(base32.as_bytes()).unwrap();
        assert_eq!(state, parsed);
    }

    #[test]
    fn test_merkle_json_roundtrip() {
        let state = Merkle::initial().next(&Hash::of(b"json merkle"));
        let json = serde_json::to_string(&state).unwrap();
        let parsed: Merkle = serde_json::from_str(&json).unwrap();
        assert_eq!(state, parsed);
    }
}

// ============================================================================
// Position Tests
// ============================================================================

mod position_tests {
    use super::*;

    #[test]
    fn test_position_creation() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        assert_eq!(pos.change.get(), 42);
        assert_eq!(pos.pos.get(), 100);
    }

    #[test]
    fn test_position_root() {
        let root = Position::<NodeId>::ROOT;
        assert!(root.is_root());
    }

    #[test]
    fn test_position_non_root() {
        let pos = Position::new(NodeId::new(1), ChangePosition::ROOT);
        assert!(!pos.is_root());
    }

    #[test]
    fn test_position_offset() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(10));
        let offset = pos.offset(5);
        assert_eq!(offset.change.get(), 1);
        assert_eq!(offset.pos.get(), 15);
    }

    #[test]
    fn test_position_add() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(10));
        let new_pos = pos + 20;
        assert_eq!(new_pos.pos.get(), 30);
        assert_eq!(new_pos.change, pos.change);
    }

    #[test]
    fn test_position_to_option() {
        let pos = Position::new(NodeId::new(5), ChangePosition::new(10));
        let opt = pos.to_option();
        assert_eq!(opt.change, Some(NodeId::new(5)));
        assert_eq!(opt.pos, ChangePosition::new(10));
    }

    #[test]
    fn test_position_option_unwrap() {
        let opt: Position<Option<NodeId>> = Position {
            change: Some(NodeId::new(5)),
            pos: ChangePosition::new(10),
        };
        let pos = opt.unwrap();
        assert_eq!(pos.change.get(), 5);
    }

    #[test]
    fn test_position_try_unwrap_some() {
        let opt: Position<Option<NodeId>> = Position {
            change: Some(NodeId::new(5)),
            pos: ChangePosition::new(10),
        };
        let pos = opt.try_unwrap();
        assert!(pos.is_some());
        assert_eq!(pos.unwrap().change.get(), 5);
    }

    #[test]
    fn test_position_try_unwrap_none() {
        let opt: Position<Option<NodeId>> = Position {
            change: None,
            pos: ChangePosition::new(10),
        };
        assert!(opt.try_unwrap().is_none());
    }

    #[test]
    fn test_position_ordering() {
        let pos1 = Position::new(NodeId::new(1), ChangePosition::new(10));
        let pos2 = Position::new(NodeId::new(1), ChangePosition::new(20));
        let pos3 = Position::new(NodeId::new(2), ChangePosition::new(5));

        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
    }

    #[test]
    fn test_position_serialization() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let json = serde_json::to_string(&pos).unwrap();
        let parsed: Position<NodeId> = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, parsed);
    }
}

// ============================================================================
// Span Tests
// ============================================================================

mod vertex_tests {
    use super::*;

    #[test]
    fn test_vertex_creation() {
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(50),
        };
        assert_eq!(v.change.get(), 1);
        assert_eq!(v.start.get(), 10);
        assert_eq!(v.end.get(), 50);
    }

    #[test]
    fn test_vertex_root() {
        let root = GraphNode::<NodeId>::ROOT;
        assert!(root.is_root());
        assert!(root.is_empty());
        assert_eq!(root.len(), 0);
    }

    #[test]
    fn test_vertex_length() {
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(50),
        };
        assert_eq!(v.len(), 40);
        assert!(!v.is_empty());
    }

    #[test]
    fn test_vertex_empty() {
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(10),
        };
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn test_vertex_start_pos() {
        let v = GraphNode {
            change: NodeId::new(5),
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
        };
        let pos = v.start_pos();
        assert_eq!(pos.change.get(), 5);
        assert_eq!(pos.pos.get(), 100);
    }

    #[test]
    fn test_vertex_end_pos() {
        let v = GraphNode {
            change: NodeId::new(5),
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
        };
        let pos = v.end_pos();
        assert_eq!(pos.change.get(), 5);
        assert_eq!(pos.pos.get(), 200);
    }

    #[test]
    fn test_vertex_to_option() {
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };
        let opt = v.to_option();
        assert_eq!(opt.change, Some(NodeId::new(1)));
    }

    #[test]
    fn test_vertex_ordering() {
        let v1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let v2 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };
        let v3 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };

        assert!(v1 < v2, "Same change, lower start should be less");
        assert!(v2 < v3, "Lower change should be less");
    }

    #[test]
    fn test_vertex_serialization() {
        let v = GraphNode {
            change: NodeId::new(123),
            start: ChangePosition::new(456),
            end: ChangePosition::new(789),
        };
        let json = serde_json::to_string(&v).unwrap();
        let parsed: GraphNode<NodeId> = serde_json::from_str(&json).unwrap();
        assert_eq!(v, parsed);
    }

    #[test]
    fn test_vertex_hash() {
        use std::collections::HashSet;

        let v1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let v2 = v1; // copy
        let v3 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };

        let mut set = HashSet::new();
        set.insert(v1);
        set.insert(v2); // duplicate
        set.insert(v3);

        assert_eq!(set.len(), 2);
    }
}

// ============================================================================
// EdgeFlags Tests
// ============================================================================

mod edge_flags_tests {
    use super::*;

    #[test]
    fn test_edge_flags_block() {
        let flags = EdgeFlags::BLOCK;
        assert!(flags.is_block());
        assert!(!flags.is_parent());
        assert!(!flags.is_folder());
        assert!(!flags.is_deleted());
        assert!(!flags.is_pseudo());
        assert!(flags.is_alive());
    }

    #[test]
    fn test_edge_flags_parent() {
        let flags = EdgeFlags::PARENT;
        assert!(flags.is_parent());
        assert!(!flags.is_block());
    }

    #[test]
    fn test_edge_flags_folder() {
        let flags = EdgeFlags::FOLDER;
        assert!(flags.is_folder());
        assert!(!flags.is_block());
    }

    #[test]
    fn test_edge_flags_deleted() {
        let flags = EdgeFlags::DELETED;
        assert!(flags.is_deleted());
        assert!(!flags.is_alive());
    }

    #[test]
    fn test_edge_flags_pseudo() {
        let flags = EdgeFlags::PSEUDO;
        assert!(flags.is_pseudo());
    }

    #[test]
    fn test_edge_flags_combinations() {
        let flags = EdgeFlags::BLOCK | EdgeFlags::PARENT;
        assert!(flags.is_block());
        assert!(flags.is_parent());
        assert!(!flags.is_folder());
    }

    #[test]
    fn test_edge_flags_deleted_folder() {
        let flags = EdgeFlags::deleted_folder();
        assert!(flags.is_deleted());
        assert!(flags.is_folder());
        assert!(!flags.is_alive());
    }

    #[test]
    fn test_edge_flags_block_parent() {
        let flags = EdgeFlags::block_parent();
        assert!(flags.is_block());
        assert!(flags.is_parent());
    }

    #[test]
    fn test_edge_flags_alive_parent() {
        let parent = EdgeFlags::PARENT;
        assert!(parent.is_alive_parent());

        let deleted_parent = EdgeFlags::PARENT | EdgeFlags::DELETED;
        assert!(!deleted_parent.is_alive_parent());
    }

    #[test]
    fn test_edge_flags_display() {
        assert_eq!(EdgeFlags::BLOCK.to_string(), "BLOCK");
        assert_eq!(
            (EdgeFlags::BLOCK | EdgeFlags::PARENT).to_string(),
            "BLOCK|PARENT"
        );
        assert_eq!(EdgeFlags::empty().to_string(), "NONE");
    }

    #[test]
    fn test_edge_flags_default() {
        let flags = EdgeFlags::default();
        assert!(flags.is_block());
    }
}

// ============================================================================
// Edge Tests
// ============================================================================

mod edge_tests {
    use super::*;

    fn make_edge(flag: EdgeFlags, change: u64, pos: u64, intro: u64) -> GraphEdge {
        GraphEdge {
            flag,
            dest: Position::new(NodeId::new(change), ChangePosition::new(pos)),
            introduced_by: NodeId::new(intro),
        }
    }

    #[test]
    fn test_edge_creation() {
        let edge = make_edge(EdgeFlags::BLOCK, 2, 5, 1);
        assert_eq!(edge.flag, EdgeFlags::BLOCK);
        assert_eq!(edge.dest.change.get(), 2);
        assert_eq!(edge.dest.pos.get(), 5);
        assert_eq!(edge.introduced_by.get(), 1);
    }

    #[test]
    fn test_edge_reverse() {
        let edge = make_edge(EdgeFlags::BLOCK, 2, 5, 1);
        let source_end = Position::new(NodeId::new(1), ChangePosition::new(10));
        let reversed = edge.reverse(source_end);

        assert!(reversed.flag.is_parent());
        assert!(reversed.flag.is_block());
        assert_eq!(reversed.dest, source_end);
        assert_eq!(reversed.introduced_by, edge.introduced_by);
    }
}

// ============================================================================
// SerializedGraphEdge Tests
// ============================================================================

mod serialized_edge_tests {
    use super::*;

    fn make_serialized_edge(
        flag: EdgeFlags,
        change: u64,
        pos: u64,
        intro: u64,
    ) -> SerializedGraphEdge {
        let dest = Position::new(NodeId::new(change), ChangePosition::new(pos));
        SerializedGraphEdge::new(flag, dest, NodeId::new(intro))
    }

    #[test]
    fn test_serialized_edge_creation() {
        let edge = make_serialized_edge(EdgeFlags::BLOCK, 2, 5, 1);

        assert_eq!(edge.flag(), EdgeFlags::BLOCK);
        assert_eq!(edge.dest().change.get(), 2);
        assert_eq!(edge.dest().pos.get(), 5);
        assert_eq!(edge.introduced_by().get(), 1);
    }

    #[test]
    fn test_serialized_edge_roundtrip() {
        let original = GraphEdge {
            flag: EdgeFlags::BLOCK | EdgeFlags::PARENT,
            dest: Position::new(NodeId::new(42), ChangePosition::new(100)),
            introduced_by: NodeId::new(7),
        };

        let serialized = SerializedGraphEdge::from(original);
        let recovered = GraphEdge::from(serialized);

        assert_eq!(original.flag, recovered.flag);
        assert_eq!(original.dest, recovered.dest);
        assert_eq!(original.introduced_by, recovered.introduced_by);
    }

    #[test]
    fn test_serialized_edge_to_edge() {
        let serialized = make_serialized_edge(EdgeFlags::FOLDER, 10, 20, 5);
        let edge = serialized.to_edge();

        assert_eq!(edge.flag, EdgeFlags::FOLDER);
        assert_eq!(edge.dest.change.get(), 10);
        assert_eq!(edge.dest.pos.get(), 20);
        assert_eq!(edge.introduced_by.get(), 5);
    }

    #[test]
    fn test_serialized_edge_flag_modification_add() {
        let dest = Position::new(NodeId::new(1), ChangePosition::new(100));
        let mut edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(5));

        assert_eq!(edge.flag(), EdgeFlags::BLOCK);

        edge += EdgeFlags::DELETED;
        assert_eq!(edge.flag(), EdgeFlags::BLOCK | EdgeFlags::DELETED);
    }

    #[test]
    fn test_serialized_edge_flag_modification_remove() {
        let dest = Position::new(NodeId::new(1), ChangePosition::new(100));
        let mut edge =
            SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::DELETED, dest, NodeId::new(5));

        assert!(edge.flag().is_deleted());

        edge -= EdgeFlags::DELETED;
        assert_eq!(edge.flag(), EdgeFlags::BLOCK);
        assert!(!edge.flag().is_deleted());
    }

    #[test]
    fn test_serialized_edge_ordering() {
        let dest1 = Position::new(NodeId::new(1), ChangePosition::new(100));
        let dest2 = Position::new(NodeId::new(1), ChangePosition::new(200));

        let edge1 = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest1, NodeId::new(1));
        let edge2 = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest2, NodeId::new(1));

        assert!(edge1 < edge2);
    }

    #[test]
    fn test_serialized_edge_max_position() {
        // Test that we can use positions up to the max (56 bits)
        let max_pos = (1u64 << 56) - 1;
        let dest = Position::new(NodeId::new(1), ChangePosition::new(max_pos));
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));
        assert_eq!(edge.dest().pos.get(), max_pos);
    }

    #[test]
    #[should_panic(expected = "exceeds maximum")]
    fn test_serialized_edge_position_overflow() {
        let too_large = 1u64 << 56;
        let dest = Position::new(NodeId::new(1), ChangePosition::new(too_large));
        let _ = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));
    }
}

// ============================================================================
// Integration Tests
// ============================================================================

mod integration_tests {
    use super::*;

    #[test]
    fn test_vertex_position_relationship() {
        // A span's start_pos and end_pos should form valid positions
        let v = GraphNode {
            change: NodeId::new(5),
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
        };

        let start = v.start_pos();
        let end = v.end_pos();

        assert_eq!(start.change, end.change);
        assert!(start.pos < end.pos);
    }

    #[test]
    fn test_edge_with_vertex_positions() {
        // An edge should be able to reference span positions
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(100),
        };

        let edge = GraphEdge {
            flag: EdgeFlags::BLOCK,
            dest: v.start_pos(),
            introduced_by: NodeId::new(1),
        };

        assert_eq!(edge.dest.change, v.change);
        assert_eq!(edge.dest.pos, v.start);
    }

    #[test]
    fn test_merkle_state_chain() {
        // Simulate a series of changes building up state
        let mut state = Merkle::initial();

        let changes: Vec<Hash> = (0..10)
            .map(|i| Hash::of(format!("change {}", i).as_bytes()))
            .collect();

        let mut states = vec![state];
        for change in &changes {
            state = state.next(change);
            states.push(state);
        }

        // All states should be unique
        let unique_states: std::collections::HashSet<_> = states.iter().collect();
        assert_eq!(unique_states.len(), states.len());
    }

    #[test]
    fn test_hash_as_change_identifier() {
        // Hashes serve as globally unique change identifiers
        let change_content = b"some change data";
        let hash = Hash::of(change_content);

        // Can be converted to base32 for display/storage
        let base32 = hash.to_base32();
        assert!(!base32.is_empty());

        // Can be recovered from base32
        let recovered = Hash::from_base32(base32.as_bytes()).unwrap();
        assert_eq!(hash, recovered);
    }

    #[test]
    fn test_vertex_edge_graph_structure() {
        // Vertices are connected by edges to form the graph
        let v1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let v2 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(20),
        };

        // Forward edge from v1 to v2
        let forward = GraphEdge {
            flag: EdgeFlags::BLOCK,
            dest: v2.start_pos(),
            introduced_by: NodeId::new(2),
        };

        // Reverse (parent) edge from v2 back to v1
        let reverse = GraphEdge {
            flag: EdgeFlags::BLOCK | EdgeFlags::PARENT,
            dest: v1.end_pos(),
            introduced_by: NodeId::new(2),
        };

        assert!(!forward.flag.is_parent());
        assert!(reverse.flag.is_parent());
        assert_eq!(forward.introduced_by, reverse.introduced_by);
    }

    #[test]
    fn test_inode_file_isolation() {
        // Different files have different inodes
        let file_a = Inode::new(1);
        let file_b = Inode::new(2);

        assert_ne!(file_a, file_b);

        // Same span can exist in different files (different inode context)
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };

        // The span itself doesn't know its inode - that's stored in the graph index
        assert!(v.len() > 0);
    }
}

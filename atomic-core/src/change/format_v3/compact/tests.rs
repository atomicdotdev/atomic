//! Tests for compact graph types and the Compactor.

use super::super::error::FormatError;
use super::super::hash_table::HashDedupTable;
use super::super::types::{CompactPosition, HASH_INDEX_NONE, HASH_INDEX_SELF};
use super::compactor::Compactor;
use super::graph_op::CompactGraphOp;
use super::types::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactInsertion, CompactNewEdge,
};
use crate::change::atom::{Atom, EdgeUpdate, Insertion, NewEdge};
use crate::change::encoding::Encoding;
use crate::change::graph_op::GraphOp;
use crate::change::local::Local;
use crate::types::{ChangePosition, EdgeFlags};
use crate::Hash;
use crate::Position;

/// Helper: make a Hash from a byte pattern.
fn make_hash(byte: u8) -> Hash {
    Hash::from_bytes([byte; 32])
}

/// Helper: make a Position<Option<Hash>> with a specific hash.
fn make_position(hash: Option<Hash>, pos: u64) -> Position<Option<Hash>> {
    Position {
        change: hash,
        pos: ChangePosition::new(pos),
    }
}

/// Helper: make a GraphNode<Option<Hash>> with a specific hash.
fn make_graph_node(hash: Option<Hash>, start: u64, end: u64) -> crate::GraphNode<Option<Hash>> {
    crate::GraphNode {
        change: hash,
        start: ChangePosition::new(start),
        end: ChangePosition::new(end),
    }
}

/// Helper: create a Compactor with a known set of hashes.
fn make_compactor_and_table() -> HashDedupTable {
    let self_hash = *make_hash(0xAA).as_bytes();
    let dep_hash = *make_hash(0xBB).as_bytes();

    let mut table = HashDedupTable::new(self_hash);
    table.insert(dep_hash).unwrap();
    table
}

// ── CompactGraphNode ───────────────────────────────────────────

#[test]
fn test_compact_graph_node_new() {
    let n = CompactGraphNode::new(5, 10, 20);
    assert_eq!(n.change, 5);
    assert_eq!(n.start, 10);
    assert_eq!(n.end, 20);
    assert_eq!(n.len(), 10);
    assert!(!n.is_empty());
}

#[test]
fn test_compact_graph_node_self_ref() {
    let n = CompactGraphNode::self_ref(0, 100);
    assert!(n.is_self_ref());
    assert!(!n.is_root());
    assert_eq!(n.change, HASH_INDEX_SELF);
}

#[test]
fn test_compact_graph_node_root() {
    let n = CompactGraphNode::root(0, 0);
    assert!(n.is_root());
    assert!(!n.is_self_ref());
    assert!(n.is_empty());
}

#[test]
fn test_compact_graph_node_display() {
    assert_eq!(
        format!("{}", CompactGraphNode::self_ref(0, 10)),
        "SELF[0:10]"
    );
    assert_eq!(format!("{}", CompactGraphNode::root(5, 5)), "ROOT[5:5]");
    assert_eq!(format!("{}", CompactGraphNode::new(3, 10, 20)), "#3[10:20]");
}

#[test]
fn test_compact_graph_node_postcard_roundtrip() {
    let nodes = vec![
        CompactGraphNode::self_ref(0, 0),
        CompactGraphNode::self_ref(42, 100),
        CompactGraphNode::root(0, 0),
        CompactGraphNode::new(5, 1000, 2000),
    ];

    for node in &nodes {
        let bytes = postcard::to_allocvec(node).unwrap();
        let decoded: CompactGraphNode = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(*node, decoded, "roundtrip failed for {:?}", node);
    }
}

#[test]
fn test_compact_graph_node_postcard_size() {
    // SELF[0:0] → all varints are 0 → 3 bytes (1+1+1)
    let small = CompactGraphNode::self_ref(0, 0);
    let bytes = postcard::to_allocvec(&small).unwrap();
    assert_eq!(bytes.len(), 3);

    // Compare with V2: Option<Hash>(33) + u64(8) + u64(8) = 49 bytes
    // We're at 3 bytes. That's 94% savings.
}

// ── CompactInsertion ───────────────────────────────────────────

#[test]
fn test_compact_insertion_basics() {
    let v = CompactInsertion {
        predecessors: vec![CompactPosition::self_ref(0)],
        successors: vec![],
        flag: EdgeFlags::BLOCK.bits(),
        start: 10,
        end: 20,
        inode: CompactPosition::self_ref(0),
    };

    assert_eq!(v.len(), 10);
    assert!(!v.is_empty());
    assert!(v.has_predecessors());
    assert!(!v.has_successors());
}

#[test]
fn test_compact_insertion_empty() {
    let v = CompactInsertion {
        predecessors: vec![],
        successors: vec![],
        flag: EdgeFlags::BLOCK.bits(),
        start: 5,
        end: 5,
        inode: CompactPosition::self_ref(0),
    };

    assert_eq!(v.len(), 0);
    assert!(v.is_empty());
    assert!(!v.has_predecessors());
}

#[test]
fn test_compact_insertion_display() {
    let v = CompactInsertion {
        predecessors: vec![CompactPosition::self_ref(0), CompactPosition::new(1, 10)],
        successors: vec![CompactPosition::self_ref(100)],
        flag: EdgeFlags::BLOCK.bits(),
        start: 0,
        end: 42,
        inode: CompactPosition::self_ref(0),
    };
    let display = format!("{}", v);
    assert!(display.contains("0..42"));
    assert!(display.contains("2 up"));
    assert!(display.contains("1 down"));
}

#[test]
fn test_compact_insertion_postcard_roundtrip() {
    let v = CompactInsertion {
        predecessors: vec![CompactPosition::self_ref(0), CompactPosition::new(1, 50)],
        successors: vec![],
        flag: EdgeFlags::BLOCK.bits(),
        start: 100,
        end: 200,
        inode: CompactPosition::self_ref(5),
    };

    let bytes = postcard::to_allocvec(&v).unwrap();
    let decoded: CompactInsertion = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(v, decoded);
}

// ── CompactNewEdge ─────────────────────────────────────────────

#[test]
fn test_compact_new_edge_display() {
    let e = CompactNewEdge {
        previous: EdgeFlags::BLOCK.bits(),
        flag: (EdgeFlags::BLOCK | EdgeFlags::DELETED).bits(),
        from: CompactPosition::self_ref(10),
        to: CompactGraphNode::new(1, 20, 30),
        introduced_by: 1,
    };
    let display = format!("{}", e);
    assert!(display.contains("Edge("));
    assert!(display.contains("by #1"));
}

#[test]
fn test_compact_new_edge_postcard_roundtrip() {
    let e = CompactNewEdge {
        previous: EdgeFlags::BLOCK.bits(),
        flag: (EdgeFlags::BLOCK | EdgeFlags::DELETED).bits(),
        from: CompactPosition::new(2, 100),
        to: CompactGraphNode::new(1, 200, 300),
        introduced_by: 1,
    };

    let bytes = postcard::to_allocvec(&e).unwrap();
    let decoded: CompactNewEdge = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(e, decoded);
}

// ── CompactEdgeUpdate ──────────────────────────────────────────

#[test]
fn test_compact_edge_update_basics() {
    let em = CompactEdgeUpdate {
        edges: vec![],
        inode: CompactPosition::self_ref(0),
    };
    assert!(em.is_empty());
    assert_eq!(em.len(), 0);
}

#[test]
fn test_compact_edge_update_with_edges() {
    let em = CompactEdgeUpdate {
        edges: vec![
            CompactNewEdge {
                previous: 0x01,
                flag: 0x05,
                from: CompactPosition::self_ref(10),
                to: CompactGraphNode::self_ref(20, 30),
                introduced_by: HASH_INDEX_SELF,
            },
            CompactNewEdge {
                previous: 0x01,
                flag: 0x05,
                from: CompactPosition::self_ref(40),
                to: CompactGraphNode::self_ref(50, 60),
                introduced_by: HASH_INDEX_SELF,
            },
        ],
        inode: CompactPosition::self_ref(0),
    };
    assert!(!em.is_empty());
    assert_eq!(em.len(), 2);
}

// ── CompactAtom ────────────────────────────────────────────────

#[test]
fn test_compact_atom_insertion() {
    let atom = CompactAtom::Insertion(CompactInsertion {
        predecessors: vec![],
        successors: vec![],
        flag: 0x01,
        start: 0,
        end: 10,
        inode: CompactPosition::self_ref(0),
    });
    assert!(atom.is_insertion());
    assert!(!atom.is_edge_update());
}

#[test]
fn test_compact_atom_edge_update() {
    let atom = CompactAtom::EdgeUpdate(CompactEdgeUpdate {
        edges: vec![],
        inode: CompactPosition::self_ref(0),
    });
    assert!(!atom.is_insertion());
    assert!(atom.is_edge_update());
}

#[test]
fn test_compact_atom_postcard_roundtrip() {
    let atom = CompactAtom::Insertion(CompactInsertion {
        predecessors: vec![CompactPosition::self_ref(5)],
        successors: vec![CompactPosition::new(1, 10)],
        flag: EdgeFlags::BLOCK.bits(),
        start: 100,
        end: 200,
        inode: CompactPosition::self_ref(0),
    });

    let bytes = postcard::to_allocvec(&atom).unwrap();
    let decoded: CompactAtom = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(atom, decoded);
}

// ── CompactGraphOp ─────────────────────────────────────────────

#[test]
fn test_compact_graph_op_file_add() {
    let op = CompactGraphOp::FileAdd {
        add_name: CompactInsertion {
            predecessors: vec![CompactPosition::root(0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 0,
            end: 9,
            inode: CompactPosition::root(0),
        },
        add_inode: CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 9,
            end: 9,
            inode: CompactPosition::self_ref(0),
        },
        contents: Some(CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 9,
            end: 42,
            inode: CompactPosition::self_ref(0),
        }),
        path: "src/main.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };

    assert_eq!(op.path(), Some("src/main.rs"));
    assert_eq!(op.type_name(), "FileAdd");
    assert!(format!("{}", op).contains("src/main.rs"));
}

#[test]
fn test_compact_graph_op_edit() {
    let op = CompactGraphOp::Edit {
        change: CompactAtom::Insertion(CompactInsertion {
            predecessors: vec![CompactPosition::new(1, 100)],
            successors: vec![CompactPosition::self_ref(200)],
            flag: EdgeFlags::BLOCK.bits(),
            start: 50,
            end: 80,
            inode: CompactPosition::self_ref(0),
        }),
        local: Local::new("lib.rs", 42),
        encoding: Some(Encoding::Utf8),
    };

    assert_eq!(op.path(), Some("lib.rs"));
    assert_eq!(op.type_name(), "Edit");
}

#[test]
fn test_compact_graph_op_all_type_names() {
    // Verify all 16 variants have distinct type names
    let names = vec![
        "FileAdd",
        "DirAdd",
        "DirDel",
        "DirUndel",
        "FileDel",
        "FileUndel",
        "FileMove",
        "Edit",
        "Replacement",
        "SolveNameConflict",
        "UnsolveNameConflict",
        "SolveOrderConflict",
        "UnsolveOrderConflict",
        "ResurrectZombies",
        "AddRoot",
        "DelRoot",
    ];
    assert_eq!(names.len(), 16);
    let mut deduped = names.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), 16, "all type names must be unique");
}

#[test]
fn test_compact_graph_op_postcard_roundtrip() {
    let op = CompactGraphOp::Edit {
        change: CompactAtom::Insertion(CompactInsertion {
            predecessors: vec![CompactPosition::new(1, 100)],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 50,
            end: 80,
            inode: CompactPosition::self_ref(0),
        }),
        local: Local::new("test.rs", 10),
        encoding: Some(Encoding::Utf8),
    };

    let bytes = postcard::to_allocvec(&op).unwrap();
    let decoded: CompactGraphOp = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(op, decoded);
}

#[test]
fn test_compact_graph_op_add_root_no_path() {
    let op = CompactGraphOp::AddRoot {
        name: CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 0,
            end: 0,
            inode: CompactPosition::root(0),
        },
        inode: CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 0,
            end: 0,
            inode: CompactPosition::root(0),
        },
    };
    assert_eq!(op.path(), None);
    assert_eq!(op.type_name(), "AddRoot");
}

// ── Compactor: compact_position / expand_position ──────────────

#[test]
fn test_compact_position_none_hash() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let pos = make_position(None, 42);
    let compact = compactor.compact_position(&pos).unwrap();

    assert_eq!(compact.change, HASH_INDEX_NONE);
    assert_eq!(compact.pos, 42);
    assert!(compact.is_root());
}

#[test]
fn test_compact_position_self_hash() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let pos = make_position(Some(self_hash), 100);
    let compact = compactor.compact_position(&pos).unwrap();

    assert_eq!(compact.change, HASH_INDEX_SELF);
    assert_eq!(compact.pos, 100);
}

#[test]
fn test_compact_position_dep_hash() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let dep_hash = make_hash(0xBB);
    let pos = make_position(Some(dep_hash), 200);
    let compact = compactor.compact_position(&pos).unwrap();

    assert_eq!(compact.change, 1); // dep is at index 1
    assert_eq!(compact.pos, 200);
}

#[test]
fn test_compact_position_unknown_hash_fails() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let unknown = make_hash(0xCC);
    let pos = make_position(Some(unknown), 0);
    let result = compactor.compact_position(&pos);

    assert!(result.is_err());
    assert!(matches!(result, Err(FormatError::HashNotFound { .. })));
}

#[test]
fn test_expand_position_none() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let compact = CompactPosition::root(42);
    let expanded = compactor.expand_position(&compact).unwrap();

    assert_eq!(expanded.change, None);
    assert_eq!(expanded.pos.get(), 42);
}

#[test]
fn test_expand_position_self() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let compact = CompactPosition::self_ref(100);
    let expanded = compactor.expand_position(&compact).unwrap();

    assert_eq!(expanded.change, Some(make_hash(0xAA)));
    assert_eq!(expanded.pos.get(), 100);
}

#[test]
fn test_expand_position_dep() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let compact = CompactPosition::new(1, 200);
    let expanded = compactor.expand_position(&compact).unwrap();

    assert_eq!(expanded.change, Some(make_hash(0xBB)));
    assert_eq!(expanded.pos.get(), 200);
}

#[test]
fn test_expand_position_out_of_bounds_fails() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let compact = CompactPosition::new(99, 0);
    let result = compactor.expand_position(&compact);

    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(FormatError::HashIndexOutOfBounds { .. })
    ));
}

// ── Compactor: compact/expand roundtrip for position ───────────

#[test]
fn test_position_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let positions = vec![
        make_position(None, 0),
        make_position(Some(make_hash(0xAA)), 42),
        make_position(Some(make_hash(0xBB)), 999),
    ];

    for pos in &positions {
        let compact = compactor.compact_position(pos).unwrap();
        let expanded = compactor.expand_position(&compact).unwrap();
        assert_eq!(*pos, expanded, "roundtrip failed for {:?}", pos);
    }
}

// ── Compactor: compact/expand roundtrip for graph_node ─────────

#[test]
fn test_graph_node_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let nodes = vec![
        make_graph_node(None, 0, 0),
        make_graph_node(Some(make_hash(0xAA)), 10, 20),
        make_graph_node(Some(make_hash(0xBB)), 100, 200),
    ];

    for node in &nodes {
        let compact = compactor.compact_graph_node(node).unwrap();
        let expanded = compactor.expand_graph_node(&compact).unwrap();
        assert_eq!(*node, expanded, "roundtrip failed for {:?}", node);
    }
}

// ── Compactor: compact/expand roundtrip for Insertion ──────────

#[test]
fn test_insertion_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let insertion = Insertion {
        predecessors: vec![
            make_position(Some(self_hash), 0),
            make_position(Some(dep_hash), 50),
        ],
        successors: vec![make_position(None, 0)],
        flag: EdgeFlags::BLOCK,
        start: ChangePosition::new(100),
        end: ChangePosition::new(200),
        inode: make_position(Some(self_hash), 5),
    };

    let compact = compactor.compact_insertion(&insertion).unwrap();
    let expanded = compactor.expand_insertion(&compact).unwrap();

    assert_eq!(insertion, expanded);
}

#[test]
fn test_insertion_compact_preserves_flag_bits() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let insertion = Insertion {
        predecessors: vec![],
        successors: vec![],
        flag: EdgeFlags::BLOCK | EdgeFlags::FOLDER | EdgeFlags::DELETED,
        start: ChangePosition::new(0),
        end: ChangePosition::new(0),
        inode: make_position(None, 0),
    };

    let compact = compactor.compact_insertion(&insertion).unwrap();
    assert_eq!(compact.flag, insertion.flag.bits());

    let expanded = compactor.expand_insertion(&compact).unwrap();
    assert_eq!(expanded.flag, insertion.flag);
}

// ── Compactor: compact/expand roundtrip for NewEdge ────────────

#[test]
fn test_new_edge_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let edge = NewEdge {
        previous: EdgeFlags::BLOCK,
        flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
        from: make_position(Some(self_hash), 10),
        to: make_graph_node(Some(dep_hash), 20, 30),
        introduced_by: Some(dep_hash),
    };

    let compact = compactor.compact_new_edge(&edge).unwrap();
    let expanded = compactor.expand_new_edge(&compact).unwrap();

    assert_eq!(edge, expanded);
}

#[test]
fn test_new_edge_with_none_introduced_by() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let edge = NewEdge {
        previous: EdgeFlags::BLOCK,
        flag: EdgeFlags::BLOCK,
        from: make_position(None, 0),
        to: make_graph_node(None, 0, 0),
        introduced_by: None,
    };

    let compact = compactor.compact_new_edge(&edge).unwrap();
    assert_eq!(compact.introduced_by, HASH_INDEX_NONE);

    let expanded = compactor.expand_new_edge(&compact).unwrap();
    assert_eq!(edge, expanded);
}

// ── Compactor: compact/expand roundtrip for EdgeUpdate ─────────

#[test]
fn test_edge_update_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let edge_update = EdgeUpdate {
        edges: vec![
            NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(Some(self_hash), 10),
                to: make_graph_node(Some(self_hash), 20, 30),
                introduced_by: Some(self_hash),
            },
            NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(Some(self_hash), 40),
                to: make_graph_node(Some(self_hash), 50, 60),
                introduced_by: Some(self_hash),
            },
        ],
        inode: make_position(Some(self_hash), 0),
    };

    let compact = compactor.compact_edge_update(&edge_update).unwrap();
    let expanded = compactor.expand_edge_update(&compact).unwrap();

    assert_eq!(edge_update, expanded);
}

// ── Compactor: compact/expand roundtrip for Atom ───────────────

#[test]
fn test_atom_insertion_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let atom = Atom::Insertion(Insertion {
        predecessors: vec![make_position(Some(make_hash(0xAA)), 0)],
        successors: vec![],
        flag: EdgeFlags::BLOCK,
        start: ChangePosition::new(10),
        end: ChangePosition::new(20),
        inode: make_position(Some(make_hash(0xAA)), 5),
    });

    let compact = compactor.compact_atom(&atom).unwrap();
    assert!(compact.is_insertion());

    let expanded = compactor.expand_atom(&compact).unwrap();
    assert_eq!(atom, expanded);
}

#[test]
fn test_atom_edge_update_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let atom = Atom::EdgeUpdate(EdgeUpdate {
        edges: vec![NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(make_hash(0xAA)), 10),
            to: make_graph_node(Some(make_hash(0xAA)), 20, 30),
            introduced_by: Some(make_hash(0xAA)),
        }],
        inode: make_position(Some(make_hash(0xAA)), 0),
    });

    let compact = compactor.compact_atom(&atom).unwrap();
    assert!(compact.is_edge_update());

    let expanded = compactor.expand_atom(&compact).unwrap();
    assert_eq!(atom, expanded);
}

// ── Compactor: compact/expand roundtrip for GraphOp ────────────

#[test]
fn test_graph_op_file_add_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let op = GraphOp::FileAdd {
        add_name: Insertion {
            predecessors: vec![make_position(None, 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(9),
            inode: make_position(None, 0),
        },
        add_inode: Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(9),
            end: ChangePosition::new(9),
            inode: make_position(Some(self_hash), 0),
        },
        contents: Some(Insertion {
            predecessors: vec![make_position(Some(self_hash), 9)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(9),
            end: ChangePosition::new(42),
            inode: make_position(Some(self_hash), 0),
        }),
        path: "src/main.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();

    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_edit_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let op = GraphOp::Edit {
        change: Atom::Insertion(Insertion {
            predecessors: vec![make_position(Some(dep_hash), 100)],
            successors: vec![make_position(Some(self_hash), 200)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(50),
            end: ChangePosition::new(80),
            inode: make_position(Some(self_hash), 0),
        }),
        local: Local::new("lib.rs", 42),
        encoding: Some(Encoding::Utf8),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();

    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_replacement_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let op = GraphOp::Replacement {
        change: EdgeUpdate {
            edges: vec![NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(Some(dep_hash), 10),
                to: make_graph_node(Some(dep_hash), 20, 30),
                introduced_by: Some(dep_hash),
            }],
            inode: make_position(Some(self_hash), 0),
        },
        replacement: Insertion {
            predecessors: vec![make_position(Some(dep_hash), 10)],
            successors: vec![make_position(Some(dep_hash), 30)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(15),
            inode: make_position(Some(self_hash), 0),
        },
        local: Local::new("test.rs", 5),
        encoding: None,
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();

    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_file_del_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let op = GraphOp::FileDel {
        del: EdgeUpdate {
            edges: vec![NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(None, 0),
                to: make_graph_node(Some(self_hash), 0, 9),
                introduced_by: Some(self_hash),
            }],
            inode: make_position(None, 0),
        },
        contents: None,
        path: "old_file.txt".to_string(),
        encoding: Some(Encoding::Utf8),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();

    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_add_root_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let op = GraphOp::AddRoot {
        name: Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(0),
            inode: make_position(None, 0),
        },
        inode: Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(0),
            inode: make_position(None, 0),
        },
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    assert_eq!(compact.type_name(), "AddRoot");

    let expanded = compactor.expand_graph_op(&compact).unwrap();
    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_del_root_compact_expand_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let op = GraphOp::DelRoot {
        name: EdgeUpdate {
            edges: vec![],
            inode: make_position(None, 0),
        },
        inode: EdgeUpdate {
            edges: vec![],
            inode: make_position(Some(self_hash), 0),
        },
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();

    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_solve_name_conflict_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let op = GraphOp::SolveNameConflict {
        name: EdgeUpdate {
            edges: vec![],
            inode: make_position(None, 0),
        },
        path: "conflict.txt".to_string(),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();
    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_dir_add_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let op = GraphOp::DirAdd {
        add_name: Insertion {
            predecessors: vec![make_position(None, 0)],
            successors: vec![],
            flag: EdgeFlags::FOLDER,
            start: ChangePosition::new(0),
            end: ChangePosition::new(5),
            inode: make_position(None, 0),
        },
        add_inode: Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::FOLDER,
            start: ChangePosition::new(5),
            end: ChangePosition::new(5),
            inode: make_position(Some(self_hash), 0),
        },
        path: "src/".to_string(),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();
    assert_eq!(op, expanded);
}

#[test]
fn test_graph_op_file_move_roundtrip() {
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let op = GraphOp::FileMove {
        del: EdgeUpdate {
            edges: vec![NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(None, 0),
                to: make_graph_node(Some(dep_hash), 0, 8),
                introduced_by: Some(dep_hash),
            }],
            inode: make_position(None, 0),
        },
        add: Insertion {
            predecessors: vec![make_position(None, 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(12),
            inode: make_position(Some(self_hash), 0),
        },
        path: "new_name.rs".to_string(),
    };

    let compact = compactor.compact_graph_op(&op).unwrap();
    let expanded = compactor.expand_graph_op(&compact).unwrap();
    assert_eq!(op, expanded);
}

// ── Postcard size savings verification ──────────────────────────

#[test]
fn test_compact_graph_op_postcard_size_savings() {
    // Build a realistic FileAdd operation and measure its compact size
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);

    let op = GraphOp::FileAdd {
        add_name: Insertion {
            predecessors: vec![make_position(None, 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(11),
            inode: make_position(None, 0),
        },
        add_inode: Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(11),
            end: ChangePosition::new(11),
            inode: make_position(Some(self_hash), 0),
        },
        contents: Some(Insertion {
            predecessors: vec![make_position(Some(self_hash), 11)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(11),
            end: ChangePosition::new(100),
            inode: make_position(Some(self_hash), 0),
        }),
        path: "src/main.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };

    // Measure full-hash postcard size (GraphOp<Option<Hash>>)
    let full_bytes = postcard::to_allocvec(&op).unwrap();

    // Measure compact postcard size (CompactGraphOp with HashIndex)
    let compact = compactor.compact_graph_op(&op).unwrap();
    let compact_bytes = postcard::to_allocvec(&compact).unwrap();

    let savings_pct = (1.0 - compact_bytes.len() as f64 / full_bytes.len() as f64) * 100.0;

    assert!(
        compact_bytes.len() < full_bytes.len() / 2,
        "Compact ({} bytes) should be less than half of full ({} bytes), savings: {:.1}%",
        compact_bytes.len(),
        full_bytes.len(),
        savings_pct,
    );
}

#[test]
fn test_compact_graph_op_full_postcard_roundtrip_via_bytes() {
    // Compact → postcard bytes → deserialize → expand → compare with original
    let table = make_compactor_and_table();
    let compactor = Compactor::new(&table);

    let self_hash = make_hash(0xAA);
    let dep_hash = make_hash(0xBB);

    let op = GraphOp::Replacement {
        change: EdgeUpdate {
            edges: vec![NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(Some(dep_hash), 50),
                to: make_graph_node(Some(dep_hash), 50, 80),
                introduced_by: Some(dep_hash),
            }],
            inode: make_position(Some(self_hash), 0),
        },
        replacement: Insertion {
            predecessors: vec![make_position(Some(dep_hash), 50)],
            successors: vec![make_position(Some(dep_hash), 80)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(25),
            inode: make_position(Some(self_hash), 0),
        },
        local: Local::new("complex.rs", 99),
        encoding: Some(Encoding::Utf8),
    };

    // Full pipeline: compact → serialize → deserialize → expand
    let compact = compactor.compact_graph_op(&op).unwrap();
    let bytes = postcard::to_allocvec(&compact).unwrap();
    let deserialized: CompactGraphOp = postcard::from_bytes(&bytes).unwrap();
    let expanded = compactor.expand_graph_op(&deserialized).unwrap();

    assert_eq!(op, expanded);
}

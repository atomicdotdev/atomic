use super::super::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactGraphOp as Op, CompactInsertion,
    CompactNewEdge, CompactPosition,
};
use super::*;
use crate::{
    change::{Encoding, InodeAttr, InodeKind, Local},
    EdgeFlags,
};

fn p(change: u16, pos: u32) -> CompactPosition {
    CompactPosition { change, pos }
}
fn insertion() -> CompactInsertion {
    CompactInsertion {
        predecessors: vec![p(0, 0), p(u16::MAX, u32::MAX)],
        successors: vec![p(2, 17)],
        flag: 255,
        start: u32::MAX,
        end: 0,
        inode: p(u16::MAX, 9),
    }
}
fn edges() -> CompactEdgeUpdate {
    let alive = (EdgeFlags::FOLDER | EdgeFlags::BLOCK).bits();
    let dead = (EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED).bits();
    CompactEdgeUpdate {
        inode: p(0, 9),
        edges: vec![CompactNewEdge {
            previous: alive,
            flag: dead,
            from: p(0, 0),
            to: CompactGraphNode::new(1, 1, 9),
            introduced_by: u16::MAX,
        }],
    }
}
fn fixture() -> GraphSectionPayload {
    let a = insertion();
    let e = edges();
    let mut inverse = e.clone();
    for edge in &mut inverse.edges {
        std::mem::swap(&mut edge.previous, &mut edge.flag);
    }
    let local = Local::new("α\0/path", u64::MAX);
    GraphSectionPayload::new(
        "root".into(),
        vec![
            Op::FileAdd {
                add_name: a.clone(),
                add_inode: a.clone(),
                contents: Some(a.clone()),
                path: "a".into(),
                encoding: Some(Encoding::Utf8),
            },
            Op::FileAdd {
                add_name: a.clone(),
                add_inode: a.clone(),
                contents: None,
                path: "a".into(),
                encoding: None,
            },
            Op::DirAdd {
                add_name: a.clone(),
                add_inode: a.clone(),
                path: "dir".into(),
            },
            Op::DirDel {
                del: e.clone(),
                path: "dir".into(),
            },
            Op::DirUndel {
                undel: e.clone(),
                path: "dir".into(),
            },
            Op::FileDel {
                del: e.clone(),
                contents: Some(e.clone()),
                path: "a".into(),
                encoding: Some(Encoding::Binary),
            },
            Op::FileUndel {
                undel: e.clone(),
                contents: None,
                path: "a".into(),
                encoding: Some(Encoding::Latin1),
            },
            Op::FileMove {
                del: e.clone(),
                add: a.clone(),
                path: "moved".into(),
            },
            Op::Edit {
                change: CompactAtom::Insertion(a.clone()),
                local: local.clone(),
                encoding: Some(Encoding::Utf16Le),
            },
            Op::Edit {
                change: CompactAtom::EdgeUpdate(e.clone()),
                local: local.clone(),
                encoding: Some(Encoding::Utf16Be),
            },
            Op::Replacement {
                change: e.clone(),
                replacement: a.clone(),
                local: local.clone(),
                encoding: None,
            },
            Op::SolveNameConflict {
                name: e.clone(),
                path: "a".into(),
            },
            Op::UnsolveNameConflict {
                name: inverse,
                path: "a".into(),
            },
            Op::SolveOrderConflict {
                change: e.clone(),
                local: local.clone(),
            },
            Op::UnsolveOrderConflict {
                change: e.clone(),
                local: local.clone(),
            },
            Op::ResurrectZombies {
                change: e.clone(),
                local,
                encoding: None,
            },
            Op::AddRoot {
                name: a.clone(),
                inode: a,
            },
            Op::DelRoot {
                name: e.clone(),
                inode: e,
            },
            Op::SetAttr {
                inode: p(0, 9),
                path: "a".into(),
                value: InodeAttr::Mode(0o755),
            },
            Op::SetAttr {
                inode: p(0, 9),
                path: "a".into(),
                value: InodeAttr::Kind(InodeKind::Symlink),
            },
        ],
        u64::MAX,
        0,
    )
}
#[test]
fn all_graph_variants_are_exact_and_deterministic() {
    for graph in [fixture(), GraphSectionPayload::new("".into(), vec![], 0, 0)] {
        let encoded = encode(&graph).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, graph);
        assert_eq!(
            postcard::to_allocvec(&decoded).unwrap(),
            postcard::to_allocvec(&graph).unwrap()
        );
        assert_eq!(encode(&decoded).unwrap(), encoded);
    }
}
#[test]
fn malformed_graph_fields_fail_closed() {
    let encoded = encode(&fixture()).unwrap();
    for n in 0..encoded.len() {
        assert!(decode(&encoded[..n]).is_err(), "n={n}");
    }
    let mut extra = encoded.clone();
    extra.push(0);
    assert!(decode(&extra).is_err());
    let mut version = encoded;
    version[4] = 2;
    assert!(decode(&version).is_err());
    let make = |control, columns, dictionary| {
        let p = wire::Payload {
            path: "",
            start: 0,
            end: 0,
            control,
            columns,
            dictionary,
            aux: vec![],
        };
        let mut out = wire::MAGIC.to_vec();
        out.extend(postcard::to_allocvec(&p).unwrap());
        out
    };
    assert!(decode(&make(vec![1, 255], vec![], vec![])).is_err());
    let mut count = vec![];
    wire::put(&mut count, u64::MAX);
    assert!(decode(&make(count, vec![], vec![])).is_err());
    assert!(decode(&make(
        vec![0],
        vec![wire::Column {
            role: 0,
            count: 1,
            constant: None,
            data: vec![0]
        }],
        vec![]
    ))
    .is_err());
    assert!(decode(&make(
        vec![0],
        vec![
            wire::Column {
                role: 0,
                count: 1,
                constant: None,
                data: vec![0]
            },
            wire::Column {
                role: 0,
                count: 1,
                constant: None,
                data: vec![0]
            }
        ],
        vec![]
    ))
    .is_err());
    assert!(decode(&make(vec![0], vec![], vec!["a".into(), "a".into()])).is_err());
    assert!(decode(&make(vec![0], vec![], vec!["unused".into()])).is_err());
}
#[test]
fn graph_selection_keeps_smaller_raw_encoding() {
    let graph = GraphSectionPayload::new("".into(), vec![], 0, 0);
    let auto = select(&graph, GraphEncoding::Auto).unwrap();
    let raw = select(&graph, GraphEncoding::Raw).unwrap();
    assert!(auto.compressed.len() <= raw.compressed.len());
    let mut graph = fixture();
    graph.ops.last_mut().map(|op| {
        *op = Op::SetAttr {
            inode: p(0, 0),
            path: "a".into(),
            value: InodeAttr::Mode(0o1000),
        }
    });
    assert!(encode(&graph).is_err());
}

#[test]
fn frozen_graph_field_wire_vector() {
    let bytes = encode(&fixture()).unwrap();
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        "ab217a8caaabcef1ca60705156120a63376c711632e7e3aad992d4afa770e96d"
    );
    assert_eq!(decode(&bytes).unwrap(), fixture());
}

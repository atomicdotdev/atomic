use crate::pristine::traits::vertex_ext::VertexExt;
use crate::types::{GraphNode, NodeId};

#[test]
fn test_vertex_from_parts() {
    let v = GraphNode::from_parts(NodeId::new(42), 100, 200);
    assert_eq!(v.change.get(), 42);
    assert_eq!(v.start.get(), 100);
    assert_eq!(v.end.get(), 200);
}

#[test]
fn test_vertex_from_parts_empty() {
    let v = GraphNode::from_parts(NodeId::new(1), 50, 50);
    assert!(v.is_empty());
    assert_eq!(v.len(), 0);
}

#[test]
fn test_vertex_from_parts_length() {
    let v = GraphNode::from_parts(NodeId::new(1), 10, 60);
    assert!(!v.is_empty());
    assert_eq!(v.len(), 50);
}

//! Extension trait for convenient vertex (graph node) creation.

use crate::types::{ChangePosition, GraphNode, NodeId};

/// Extension trait for convenient span creation
///
/// Provides a helper method for creating vertices from their component parts.
///
/// # Example
///
/// ```
/// use atomic_core::pristine::VertexExt;
/// use atomic_core::types::{NodeId, GraphNode};
///
/// let node = GraphNode::from_parts(NodeId::new(42), 100, 200);
/// assert_eq!(node.change.get(), 42);
/// assert_eq!(node.start.get(), 100);
/// assert_eq!(node.end.get(), 200);
/// ```
pub trait VertexExt {
    /// Create a span from component parts
    ///
    /// # Arguments
    ///
    /// * `change_id` - The change that introduced this span
    /// * `start` - Start position (inclusive)
    /// * `end` - End position (exclusive)
    fn from_parts(change_id: NodeId, start: u64, end: u64) -> GraphNode<NodeId>;
}

impl VertexExt for GraphNode<NodeId> {
    fn from_parts(change_id: NodeId, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode {
            change: change_id,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }
}

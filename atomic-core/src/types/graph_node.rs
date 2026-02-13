//! GraphNode type for the Atomic graph
//!
//! A GraphNode represents a contiguous chunk of content within the repository
//! graph. GraphNodes are the nodes in our DAG, connected by GraphEdges that define
//! ordering relationships.

use super::node_id::{ChangePosition, NodeId, L64};
use super::position::Position;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A node in the repository graph.
///
/// A GraphNode represents a contiguous range of bytes within a change's content.
/// The content itself is stored in the change file; the node just references
/// it by position.
///
/// # Type Parameter
///
/// - `H`: The type used to identify the change. This is typically `NodeId` for
///   internal operations or `Hash` for external (serialized) references.
///
/// # Layout
///
/// The `#[repr(C)]` ensures consistent memory layout for storage and
/// comparison operations.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GraphNode<H> {
    /// The change that introduced this node
    pub change: H,
    /// Start position within the change's content (inclusive)
    pub start: ChangePosition,
    /// End position within the change's content (exclusive)
    pub end: ChangePosition,
}

impl<H: fmt::Debug> fmt::Debug for GraphNode<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "N({:?}[{}:{}])",
            self.change,
            self.start.get(),
            self.end.get()
        )
    }
}

impl<H: fmt::Display> fmt::Display for GraphNode<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}[{}:{}]",
            self.change,
            self.start.get(),
            self.end.get()
        )
    }
}

impl GraphNode<NodeId> {
    /// The root node of the repository graph.
    ///
    /// Every repository has a single root node from which all content
    /// is reachable. This is the starting point for graph traversal.
    pub const ROOT: GraphNode<NodeId> = GraphNode {
        change: NodeId::ROOT,
        start: ChangePosition::ROOT,
        end: ChangePosition::ROOT,
    };

    /// The maximum possible node (used for range queries).
    pub const MAX: GraphNode<NodeId> = GraphNode {
        change: NodeId::MAX,
        start: ChangePosition(L64(u64::MAX)),
        end: ChangePosition(L64(u64::MAX)),
    };

    /// The "bottom" sentinel node.
    ///
    /// This is used for special graph operations, particularly for
    /// representing the end of file content.
    pub const BOTTOM: GraphNode<NodeId> = GraphNode {
        change: NodeId::ROOT,
        start: ChangePosition::BOTTOM,
        end: ChangePosition::BOTTOM,
    };

    /// Check if this is the root node.
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Get the root node.
    ///
    /// This is a convenience method equivalent to `GraphNode::ROOT`.
    #[inline]
    pub fn root() -> Self {
        Self::ROOT
    }

    /// Convert to a node with optional change ID.
    ///
    /// This is useful when converting between internal and external
    /// representations.
    pub fn to_option(&self) -> GraphNode<Option<NodeId>> {
        GraphNode {
            change: Some(self.change),
            start: self.start,
            end: self.end,
        }
    }
}

impl<H: Clone> GraphNode<H> {
    /// Create a new graph node.
    #[inline]
    pub fn new(change: H, start: ChangePosition, end: ChangePosition) -> Self {
        GraphNode { change, start, end }
    }

    /// Get the start position as a `Position`.
    ///
    /// This is useful for edge references that point to the beginning
    /// of a node.
    #[inline]
    pub fn start_pos(&self) -> Position<H> {
        Position {
            change: self.change.clone(),
            pos: self.start,
        }
    }

    /// Get the end position as a `Position`.
    ///
    /// This is useful for edge references that point to the end
    /// of a node.
    #[inline]
    pub fn end_pos(&self) -> Position<H> {
        Position {
            change: self.change.clone(),
            pos: self.end,
        }
    }

    /// Check if this node is empty (zero length).
    ///
    /// Empty nodes are used for structural markers like file inodes
    /// and directory entries.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Get the length of this node in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

impl<H> GraphNode<Option<H>> {
    /// Unwrap the optional change ID.
    ///
    /// # Panics
    ///
    /// Panics if the change ID is None.
    pub fn unwrap(self) -> GraphNode<H> {
        GraphNode {
            change: self.change.unwrap(),
            start: self.start,
            end: self.end,
        }
    }

    /// Try to unwrap the optional change ID.
    pub fn try_unwrap(self) -> Option<GraphNode<H>> {
        Some(GraphNode {
            change: self.change?,
            start: self.start,
            end: self.end,
        })
    }
}

/// Trait for types that can be converted to a GraphNode.
pub trait IntoGraphNode<H> {
    /// Convert to a GraphNode.
    fn into_graph_node(self) -> GraphNode<H>;
}

impl<H> IntoGraphNode<H> for GraphNode<H> {
    #[inline]
    fn into_graph_node(self) -> GraphNode<H> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_node() {
        assert!(GraphNode::<NodeId>::ROOT.is_root());
        assert!(GraphNode::<NodeId>::ROOT.is_empty());
        assert_eq!(GraphNode::<NodeId>::ROOT.len(), 0);
    }

    #[test]
    fn test_node_length() {
        let n = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(50),
        };
        assert_eq!(n.len(), 40);
        assert!(!n.is_empty());
    }

    #[test]
    fn test_node_positions() {
        let n = GraphNode {
            change: NodeId::new(5),
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
        };

        let start_pos = n.start_pos();
        assert_eq!(start_pos.change, NodeId::new(5));
        assert_eq!(start_pos.pos, ChangePosition::new(100));

        let end_pos = n.end_pos();
        assert_eq!(end_pos.change, NodeId::new(5));
        assert_eq!(end_pos.pos, ChangePosition::new(200));
    }

    #[test]
    fn test_node_ordering() {
        let n1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let n2 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };
        let n3 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };

        assert!(n1 < n2); // Same change, different start
        assert!(n2 < n3); // Different change
    }

    #[test]
    fn test_to_option() {
        let n = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };

        let opt = n.to_option();
        assert_eq!(opt.change, Some(NodeId::new(1)));
        assert_eq!(opt.start, ChangePosition::new(10));
        assert_eq!(opt.end, ChangePosition::new(20));
    }

    #[test]
    fn test_unwrap_option() {
        let opt = GraphNode {
            change: Some(NodeId::new(1)),
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };

        let n = opt.unwrap();
        assert_eq!(n.change, NodeId::new(1));
    }

    #[test]
    fn test_try_unwrap_none() {
        let opt: GraphNode<Option<NodeId>> = GraphNode {
            change: None,
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
        };

        assert!(opt.try_unwrap().is_none());
    }

    #[test]
    fn test_node_debug() {
        let n = GraphNode {
            change: NodeId::new(42),
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
        };

        let debug = format!("{:?}", n);
        assert!(debug.contains("42"));
        assert!(debug.contains("100"));
        assert!(debug.contains("200"));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let n = GraphNode {
            change: NodeId::new(123),
            start: ChangePosition::new(456),
            end: ChangePosition::new(789),
        };

        let json = serde_json::to_string(&n).unwrap();
        let parsed: GraphNode<NodeId> = serde_json::from_str(&json).unwrap();
        assert_eq!(n, parsed);
    }

    #[test]
    fn test_node_hash() {
        use std::collections::HashSet;

        let n1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let n2 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let n3 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };

        let mut set = HashSet::new();
        set.insert(n1);
        set.insert(n2); // Should not add (duplicate)
        set.insert(n3);

        assert_eq!(set.len(), 2);
    }
}

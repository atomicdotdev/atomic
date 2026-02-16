//! Alive vertex types for graph traversal
//!
//! This module defines the vertex representation used during graph traversal
//! for file output. Each `AliveVertex` wraps a `GraphNode` with additional
//! metadata needed for:
//!
//! - **Classification**: Is the node alive, deleted, or a zombie?
//! - **Traversal state**: Has it been visited? Is it on the stack?
//! - **SCC computation**: Index and lowlink values for Tarjan's algorithm
//! - **Child tracking**: Pointers into the shared children array
//!
//! # Vertex States
//!
//! During traversal, vertices can be in several states:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Vertex State Machine                             │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │   ┌──────────┐     visit      ┌──────────┐    finish    ┌──────────┐   │
//! │   │ Unvisited│ ──────────────▶│ On Stack │ ────────────▶│ Complete │   │
//! │   └──────────┘                └──────────┘              └──────────┘   │
//! │                                    │                                    │
//! │                                    │ (cycle detected)                   │
//! │                                    ▼                                    │
//! │                               ┌──────────┐                              │
//! │                               │ In SCC   │                              │
//! │                               └──────────┘                              │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Zombie Vertices
//!
//! A zombie vertex is one that has been deleted by one change but still has
//! live (non-deleted) parent edges from another change. This indicates a
//! conflict that needs user resolution.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::alive::{AliveVertex, VertexFlags, VertexId};
//! use atomic_core::types::{NodeId, GraphNode, ChangePosition};
//!
//! // Create a GraphNode for a content chunk
//! let graph_node = GraphNode::new(
//!     NodeId::new(42),
//!     ChangePosition::new(0),
//!     ChangePosition::new(100),
//! );
//!
//! let alive = AliveVertex::new(graph_node);
//! assert!(!alive.is_zombie());
//! assert!(!alive.is_visited());
//!
//! // Mark as zombie (deleted but with live connections)
//! let zombie = alive.with_flags(VertexFlags::ZOMBIE);
//! assert!(zombie.is_zombie());
//! ```

use crate::types::{GraphNode, NodeId, SerializedGraphEdge};
use bitflags::bitflags;

// VERTEX ID

/// Index into the alive graph's span array.
///
/// This is a lightweight handle used to reference vertices without copying
/// the full span data. It's essentially an array index with a distinct type
/// for type safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VertexId(pub usize);

impl VertexId {
    /// A dummy span ID used as a sentinel value.
    ///
    /// This is typically used to mark the end of child lists or represent
    /// the "bottom" of the graph.
    pub const DUMMY: VertexId = VertexId(0);

    /// Create a new span ID.
    #[inline]
    pub fn new(index: usize) -> Self {
        VertexId(index)
    }

    /// Get the underlying index.
    #[inline]
    pub fn index(self) -> usize {
        self.0
    }

    /// Check if this is the dummy span.
    #[inline]
    pub fn is_dummy(self) -> bool {
        self == Self::DUMMY
    }
}

impl From<usize> for VertexId {
    fn from(index: usize) -> Self {
        VertexId(index)
    }
}

impl From<VertexId> for usize {
    fn from(id: VertexId) -> Self {
        id.0
    }
}

impl std::fmt::Display for VertexId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "V{}", self.0)
    }
}

// VERTEX FLAGS

bitflags! {
    /// Flags describing the state of an alive span during traversal.
    ///
    /// These flags are used by the graph traversal algorithms (DFS, Tarjan)
    /// to track state and by the output process to handle special cases.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct VertexFlags: u8 {
        /// The span has been visited during traversal.
        ///
        /// Set when DFS first reaches this span.
        const VISITED = 0b0000_0001;

        /// The span is currently on the DFS stack.
        ///
        /// Used to detect cycles during Tarjan's algorithm.
        const ON_STACK = 0b0000_0010;

        /// The span is a zombie (deleted with live connections).
        ///
        /// This indicates a conflict: the content was deleted by one change
        /// but modified by another. It will be output with conflict markers.
        const ZOMBIE = 0b0000_0100;

        /// The span has been fully processed.
        ///
        /// All children have been visited and SCC computed.
        const COMPLETE = 0b0000_1000;
    }
}

impl VertexFlags {
    /// Check if the visited flag is set.
    #[inline]
    pub fn is_visited(self) -> bool {
        self.contains(Self::VISITED)
    }

    /// Check if the span is on the stack.
    #[inline]
    pub fn is_on_stack(self) -> bool {
        self.contains(Self::ON_STACK)
    }

    /// Check if the span is a zombie.
    #[inline]
    pub fn is_zombie(self) -> bool {
        self.contains(Self::ZOMBIE)
    }

    /// Check if the span is complete.
    #[inline]
    pub fn is_complete(self) -> bool {
        self.contains(Self::COMPLETE)
    }
}

impl std::fmt::Display for VertexFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if self.contains(Self::VISITED) {
            parts.push("VISITED");
        }
        if self.contains(Self::ON_STACK) {
            parts.push("ON_STACK");
        }
        if self.contains(Self::ZOMBIE) {
            parts.push("ZOMBIE");
        }
        if self.contains(Self::COMPLETE) {
            parts.push("COMPLETE");
        }
        if parts.is_empty() {
            write!(f, "NONE")
        } else {
            write!(f, "{}", parts.join("|"))
        }
    }
}

// ALIVE VERTEX

/// A span in the alive graph with traversal metadata.
///
/// This structure wraps a graph span with additional fields needed for:
///
/// - **Tarjan's SCC algorithm**: `index`, `lowlink`, `scc`
/// - **Child management**: `children`, `n_children`, `extra`
/// - **State tracking**: `flags`
///
/// # Memory Layout
///
/// The struct is designed for efficient access during traversal:
///
/// ```text
/// AliveVertex (72 bytes typical)
/// ├── span: GraphNode<NodeId>     (24 bytes)
/// ├── flags: VertexFlags         (1 byte, padded)
/// ├── children: usize            (8 bytes)
/// ├── n_children: usize          (8 bytes)
/// ├── index: usize               (8 bytes)
/// ├── lowlink: usize             (8 bytes)
/// ├── scc: usize                 (8 bytes)
/// └── extra: Vec<...>            (~24 bytes)
/// ```
#[derive(Debug, Clone)]
pub struct AliveVertex {
    /// The underlying graph node.
    pub node: GraphNode<NodeId>,

    /// Flags indicating vertex state.
    flags: VertexFlags,

    /// Index into the shared children array where this vertex's children start.
    pub children: usize,

    /// Number of children for this vertex.
    pub n_children: usize,

    /// DFS discovery index (for Tarjan's algorithm).
    pub index: usize,

    /// Lowest reachable index (for Tarjan's algorithm).
    pub lowlink: usize,

    /// Strongly connected component ID.
    pub scc: usize,

    /// Extra children added after initial collection.
    ///
    /// This is used when edges are discovered during later processing.
    pub extra: Vec<(Option<SerializedGraphEdge>, VertexId)>,
}

impl AliveVertex {
    /// A dummy vertex used as a sentinel.
    ///
    /// This is placed at index 0 in the graph to serve as a null/bottom value.
    pub const DUMMY: AliveVertex = AliveVertex {
        node: GraphNode::BOTTOM,
        flags: VertexFlags::empty(),
        children: 0,
        n_children: 0,
        index: 0,
        lowlink: 0,
        scc: 0,
        extra: Vec::new(),
    };

    /// Create a new alive vertex from a graph node.
    ///
    /// The vertex starts with no flags set and no children.
    ///
    /// # Arguments
    ///
    /// * `node` - The underlying graph node
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::alive::AliveVertex;
    /// use atomic_core::types::{NodeId, GraphNode, ChangePosition};
    ///
    /// let v = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(10));
    /// let alive = AliveVertex::new(v);
    /// assert_eq!(alive.node.len(), 10);
    /// ```
    pub fn new(node: GraphNode<NodeId>) -> Self {
        AliveVertex {
            node,
            flags: VertexFlags::empty(),
            children: 0,
            n_children: 0,
            index: 0,
            lowlink: 0,
            scc: 0,
            extra: Vec::new(),
        }
    }

    /// Create an alive vertex with specific flags.
    ///
    /// This is useful for creating zombie vertices.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::alive::{AliveVertex, VertexFlags};
    /// use atomic_core::types::{NodeId, GraphNode, ChangePosition};
    ///
    /// let v = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(10));
    /// let zombie = AliveVertex::new(v).with_flags(VertexFlags::ZOMBIE);
    /// assert!(zombie.is_zombie());
    /// ```
    pub fn with_flags(mut self, flags: VertexFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Get the current flags.
    #[inline]
    pub fn flags(&self) -> VertexFlags {
        self.flags
    }

    /// Set flags on this vertex.
    #[inline]
    pub fn set_flags(&mut self, flags: VertexFlags) {
        self.flags = flags;
    }

    /// Add flags to this vertex.
    #[inline]
    pub fn add_flags(&mut self, flags: VertexFlags) {
        self.flags |= flags;
    }

    /// Remove flags from this vertex.
    #[inline]
    pub fn remove_flags(&mut self, flags: VertexFlags) {
        self.flags &= !flags;
    }

    /// Check if this vertex has been visited.
    #[inline]
    pub fn is_visited(&self) -> bool {
        self.flags.is_visited()
    }

    /// Check if this vertex is on the DFS stack.
    #[inline]
    pub fn is_on_stack(&self) -> bool {
        self.flags.is_on_stack()
    }

    /// Check if this vertex is a zombie.
    #[inline]
    pub fn is_zombie(&self) -> bool {
        self.flags.is_zombie()
    }

    /// Check if this vertex is complete (fully processed).
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.flags.is_complete()
    }

    /// Mark this vertex as visited.
    #[inline]
    pub fn mark_visited(&mut self) {
        self.flags |= VertexFlags::VISITED;
    }

    /// Push this vertex onto the stack.
    #[inline]
    pub fn push_stack(&mut self) {
        self.flags |= VertexFlags::ON_STACK;
    }

    /// Pop this vertex from the stack.
    #[inline]
    pub fn pop_stack(&mut self) {
        self.flags &= !VertexFlags::ON_STACK;
    }

    /// Mark this vertex as complete.
    #[inline]
    pub fn mark_complete(&mut self) {
        self.flags |= VertexFlags::COMPLETE;
    }

    /// Mark this vertex as a zombie.
    #[inline]
    pub fn mark_zombie(&mut self) {
        self.flags |= VertexFlags::ZOMBIE;
    }

    /// Get the length of this vertex's content in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.node.len()
    }

    /// Check if this vertex is empty (zero length).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.node.is_empty()
    }

    /// Get the total number of children (regular + extra).
    #[inline]
    pub fn total_children(&self) -> usize {
        self.n_children + self.extra.len()
    }

    /// Check if this is the dummy vertex.
    #[inline]
    pub fn is_dummy(&self) -> bool {
        self.node == GraphNode::BOTTOM && self.n_children == 0
    }
}

impl Default for AliveVertex {
    fn default() -> Self {
        Self::DUMMY
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -------------------------------------------------------------------------
    // VertexId Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_vertex_id_new() {
        let id = VertexId::new(42);
        assert_eq!(id.index(), 42);
    }

    #[test]
    fn test_vertex_id_dummy() {
        assert!(VertexId::DUMMY.is_dummy());
        assert!(!VertexId::new(1).is_dummy());
    }

    #[test]
    fn test_vertex_id_from_usize() {
        let id: VertexId = 100.into();
        assert_eq!(id.index(), 100);
    }

    #[test]
    fn test_vertex_id_into_usize() {
        let id = VertexId::new(50);
        let index: usize = id.into();
        assert_eq!(index, 50);
    }

    #[test]
    fn test_vertex_id_display() {
        let id = VertexId::new(7);
        assert_eq!(id.to_string(), "V7");
    }

    #[test]
    fn test_vertex_id_ordering() {
        let v1 = VertexId::new(1);
        let v2 = VertexId::new(2);
        let v3 = VertexId::new(1);

        assert!(v1 < v2);
        assert_eq!(v1, v3);
    }

    #[test]
    fn test_vertex_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(VertexId::new(1));
        set.insert(VertexId::new(2));
        set.insert(VertexId::new(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    // -------------------------------------------------------------------------
    // VertexFlags Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_vertex_flags_default() {
        let flags = VertexFlags::default();
        assert!(flags.is_empty());
        assert!(!flags.is_visited());
        assert!(!flags.is_on_stack());
        assert!(!flags.is_zombie());
        assert!(!flags.is_complete());
    }

    #[test]
    fn test_vertex_flags_visited() {
        let flags = VertexFlags::VISITED;
        assert!(flags.is_visited());
        assert!(!flags.is_zombie());
    }

    #[test]
    fn test_vertex_flags_on_stack() {
        let flags = VertexFlags::ON_STACK;
        assert!(flags.is_on_stack());
    }

    #[test]
    fn test_vertex_flags_zombie() {
        let flags = VertexFlags::ZOMBIE;
        assert!(flags.is_zombie());
    }

    #[test]
    fn test_vertex_flags_complete() {
        let flags = VertexFlags::COMPLETE;
        assert!(flags.is_complete());
    }

    #[test]
    fn test_vertex_flags_combination() {
        let flags = VertexFlags::VISITED | VertexFlags::ZOMBIE;
        assert!(flags.is_visited());
        assert!(flags.is_zombie());
        assert!(!flags.is_on_stack());
    }

    #[test]
    fn test_vertex_flags_display() {
        assert_eq!(VertexFlags::empty().to_string(), "NONE");
        assert_eq!(VertexFlags::VISITED.to_string(), "VISITED");
        assert_eq!(
            (VertexFlags::VISITED | VertexFlags::ZOMBIE).to_string(),
            "VISITED|ZOMBIE"
        );
    }

    // -------------------------------------------------------------------------
    // AliveVertex Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_alive_vertex_new() {
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        let alive = AliveVertex::new(v);

        assert_eq!(alive.node.change, NodeId::new(1));
        assert_eq!(alive.len(), 100);
        assert!(!alive.is_visited());
        assert!(!alive.is_zombie());
    }

    #[test]
    fn test_alive_vertex_with_flags() {
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let alive = AliveVertex::new(v).with_flags(VertexFlags::ZOMBIE);

        assert!(alive.is_zombie());
    }

    #[test]
    fn test_alive_vertex_dummy() {
        assert!(AliveVertex::DUMMY.is_dummy());
        assert!(AliveVertex::DUMMY.is_empty());
    }

    #[test]
    fn test_alive_vertex_default() {
        let alive = AliveVertex::default();
        assert!(alive.is_dummy());
    }

    #[test]
    fn test_alive_vertex_mark_visited() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        assert!(!alive.is_visited());
        alive.mark_visited();
        assert!(alive.is_visited());
    }

    #[test]
    fn test_alive_vertex_stack_operations() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        assert!(!alive.is_on_stack());
        alive.push_stack();
        assert!(alive.is_on_stack());
        alive.pop_stack();
        assert!(!alive.is_on_stack());
    }

    #[test]
    fn test_alive_vertex_mark_complete() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        assert!(!alive.is_complete());
        alive.mark_complete();
        assert!(alive.is_complete());
    }

    #[test]
    fn test_alive_vertex_mark_zombie() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        assert!(!alive.is_zombie());
        alive.mark_zombie();
        assert!(alive.is_zombie());
    }

    #[test]
    fn test_alive_vertex_add_remove_flags() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        alive.add_flags(VertexFlags::VISITED | VertexFlags::ON_STACK);
        assert!(alive.is_visited());
        assert!(alive.is_on_stack());

        alive.remove_flags(VertexFlags::ON_STACK);
        assert!(alive.is_visited());
        assert!(!alive.is_on_stack());
    }

    #[test]
    fn test_alive_vertex_set_flags() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);
        alive.mark_visited();

        alive.set_flags(VertexFlags::ZOMBIE);
        assert!(!alive.is_visited()); // Previous flags cleared
        assert!(alive.is_zombie());
    }

    #[test]
    fn test_alive_vertex_total_children() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        alive.n_children = 3;
        alive.extra.push((None, VertexId::new(10)));
        alive.extra.push((None, VertexId::new(11)));

        assert_eq!(alive.total_children(), 5);
    }

    #[test]
    fn test_alive_vertex_len() {
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(10),
            ChangePosition::new(50),
        );
        let alive = AliveVertex::new(v);

        assert_eq!(alive.len(), 40);
        assert!(!alive.is_empty());
    }

    #[test]
    fn test_alive_vertex_is_empty() {
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(0),
        );
        let alive = AliveVertex::new(v);

        assert!(alive.is_empty());
    }

    #[test]
    fn test_alive_vertex_clone() {
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        let mut alive = AliveVertex::new(v);
        alive.mark_visited();
        alive.scc = 5;

        let cloned = alive.clone();
        assert!(cloned.is_visited());
        assert_eq!(cloned.scc, 5);
    }

    #[test]
    fn test_alive_vertex_debug() {
        let v = GraphNode::new(
            NodeId::new(42),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let alive = AliveVertex::new(v);

        let debug = format!("{:?}", alive);
        assert!(debug.contains("AliveVertex"));
    }

    // -------------------------------------------------------------------------
    // Tarjan Algorithm Field Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_alive_vertex_tarjan_fields() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        alive.index = 5;
        alive.lowlink = 3;
        alive.scc = 2;

        assert_eq!(alive.index, 5);
        assert_eq!(alive.lowlink, 3);
        assert_eq!(alive.scc, 2);
    }

    #[test]
    fn test_alive_vertex_children_fields() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        alive.children = 100;
        alive.n_children = 5;

        assert_eq!(alive.children, 100);
        assert_eq!(alive.n_children, 5);
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_vertex_id_zero() {
        let id = VertexId::new(0);
        assert!(id.is_dummy());
        assert_eq!(id.index(), 0);
    }

    #[test]
    fn test_vertex_id_max() {
        let id = VertexId::new(usize::MAX);
        assert!(!id.is_dummy());
        assert_eq!(id.index(), usize::MAX);
    }

    #[test]
    fn test_all_flags_combined() {
        let flags = VertexFlags::VISITED
            | VertexFlags::ON_STACK
            | VertexFlags::ZOMBIE
            | VertexFlags::COMPLETE;

        assert!(flags.is_visited());
        assert!(flags.is_on_stack());
        assert!(flags.is_zombie());
        assert!(flags.is_complete());
    }

    #[test]
    fn test_alive_vertex_extra_children() {
        let v = GraphNode::ROOT;
        let mut alive = AliveVertex::new(v);

        assert!(alive.extra.is_empty());

        alive.extra.push((None, VertexId::new(1)));
        alive.extra.push((None, VertexId::new(2)));

        assert_eq!(alive.extra.len(), 2);
        assert_eq!(alive.total_children(), 2);
    }
}

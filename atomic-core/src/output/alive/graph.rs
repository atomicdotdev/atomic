//! Alive graph data structure
//!
//! This module defines the `AliveGraph` structure which holds the traversed
//! vertices and their children during file content output. The graph is built
//! by starting from a file's inode position and following edges to discover
//! all alive (non-deleted) content.
//!
//! # Architecture
//!
//! The `AliveGraph` uses a compact representation optimized for traversal:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         AliveGraph Layout                                │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  vertices: Vec<AliveVertex>        children: Vec<(Edge?, VertexId)>     │
//! │  ┌─────────────────────────┐      ┌─────────────────────────────────┐   │
//! │  │ [0] DUMMY               │      │ [0] (None, DUMMY)               │   │
//! │  │ [1] Root span         │ ───▶ │ [1] (Some(e1), V2)              │   │
//! │  │     children: 1         │      │ [2] (Some(e2), V3)              │   │
//! │  │     n_children: 3       │      │ [3] (None, DUMMY) <- sentinel   │   │
//! │  │ [2] Content span A    │ ───▶ │ [4] (Some(e3), V4)              │   │
//! │  │     children: 4         │      │ [5] (None, DUMMY)               │   │
//! │  │     n_children: 2       │      │ ...                             │   │
//! │  │ [3] Content span B    │      └─────────────────────────────────┘   │
//! │  │ ...                     │                                            │
//! │  └─────────────────────────┘                                            │
//! │                                                                         │
//! │  total_bytes: usize   (sum of all span content lengths)               │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Child Storage
//!
//! Children are stored in a flat array for cache efficiency. Each span has:
//! - `children`: index into the children array where its children start
//! - `n_children`: number of children (including sentinel)
//! - `extra`: additional children discovered later
//!
//! The sentinel `(None, DUMMY)` marks the end of each span's child list,
//! which simplifies iteration.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::alive::{AliveGraph, AliveVertex, VertexId};
//! use atomic_core::types::{NodeId, GraphNode, ChangePosition};
//!
//! // Create an empty graph
//! let mut graph = AliveGraph::new();
//!
//! // Add the dummy span (required at index 0)
//! graph.push_vertex(AliveVertex::DUMMY);
//!
//! // Add a root span
//! let root = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(0));
//! graph.push_vertex(AliveVertex::new(root));
//!
//! assert_eq!(graph.len_vertices(), 2);
//! ```

use super::vertex::{AliveVertex, VertexFlags, VertexId};
#[allow(unused_imports)]
use crate::types::{GraphNode, NodeId, SerializedGraphEdge};

// ============================================================================
// ALIVE GRAPH
// ============================================================================

/// A graph of alive vertices for file content output.
///
/// This structure holds all the vertices discovered during graph traversal
/// along with their edge relationships. It's used to determine the order
/// in which content should be output, handling conflicts along the way.
///
/// # Invariants
///
/// - Index 0 always contains `AliveVertex::DUMMY`
/// - Each span's `children` field points into the `children` array
/// - Each span's child list ends with `(None, VertexId::DUMMY)`
#[derive(Debug)]
pub struct AliveGraph {
    /// All vertices in the graph.
    ///
    /// Index 0 is always the DUMMY span.
    vertices: Vec<AliveVertex>,

    /// Shared storage for all vertices' children.
    ///
    /// Each entry is `(Option<edge>, child_vertex_id)`.
    /// `None` edges are used for sentinels marking end of child lists.
    children: Vec<(Option<SerializedGraphEdge>, VertexId)>,

    /// Total bytes of content across all vertices.
    total_bytes: usize,
}

impl AliveGraph {
    /// Create a new, empty alive graph.
    ///
    /// The graph starts empty. You should typically push `AliveVertex::DUMMY`
    /// as the first span.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::alive::{AliveGraph, AliveVertex};
    ///
    /// let mut graph = AliveGraph::new();
    /// graph.push_vertex(AliveVertex::DUMMY);
    /// assert_eq!(graph.len_vertices(), 1);
    /// ```
    pub fn new() -> Self {
        Self {
            vertices: Vec::new(),
            children: Vec::new(),
            total_bytes: 0,
        }
    }

    /// Create a graph with pre-allocated capacity.
    ///
    /// # Arguments
    ///
    /// * `vertex_capacity` - Expected number of vertices
    /// * `children_capacity` - Expected total number of child entries
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::alive::AliveGraph;
    ///
    /// // Pre-allocate for a file with ~100 vertices
    /// let graph = AliveGraph::with_capacity(100, 200);
    /// ```
    pub fn with_capacity(vertex_capacity: usize, children_capacity: usize) -> Self {
        Self {
            vertices: Vec::with_capacity(vertex_capacity),
            children: Vec::with_capacity(children_capacity),
            total_bytes: 0,
        }
    }

    /// Get the number of vertices in the graph.
    ///
    /// This includes the DUMMY span at index 0.
    #[inline]
    pub fn len_vertices(&self) -> usize {
        self.vertices.len()
    }

    /// Check if the graph has no vertices.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// Get the total bytes of content across all vertices.
    ///
    /// This is the sum of all span lengths, useful for progress reporting
    /// or buffer pre-allocation.
    #[inline]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Get the number of child entries in the children array.
    #[inline]
    pub fn len_children(&self) -> usize {
        self.children.len()
    }

    /// Push a span onto the graph.
    ///
    /// Returns the `VertexId` assigned to this span.
    ///
    /// # Arguments
    ///
    /// * `span` - The span to add
    ///
    /// # Returns
    ///
    /// The ID that can be used to reference this span.
    pub fn push_vertex(&mut self, vertex: AliveVertex) -> VertexId {
        let id = VertexId::new(self.vertices.len());
        self.total_bytes += vertex.len();
        self.vertices.push(vertex);
        id
    }

    /// Push a child entry to the children array.
    ///
    /// This should be called while building a span's child list.
    /// Don't forget to push the sentinel `(None, VertexId::DUMMY)` at the end.
    ///
    /// # Arguments
    ///
    /// * `edge` - The edge leading to the child (or None for sentinel)
    /// * `child` - The child span ID
    pub fn push_child(&mut self, edge: Option<SerializedGraphEdge>, child: VertexId) {
        self.children.push((edge, child));
    }

    /// Push a child and increment the last span's child count.
    ///
    /// This is a convenience method that combines pushing a child and
    /// updating the count on the most recently added span.
    ///
    /// # Panics
    ///
    /// Panics if the graph has no vertices.
    pub fn push_child_to_last(&mut self, edge: Option<SerializedGraphEdge>, child: VertexId) {
        self.children.push((edge, child));
        if let Some(last) = self.vertices.last_mut() {
            last.n_children += 1;
        }
    }

    /// Set the children start index for the last span.
    ///
    /// This should be called before pushing that span's children.
    ///
    /// # Panics
    ///
    /// Panics if the graph has no vertices.
    pub fn set_last_children_start(&mut self) {
        if let Some(last) = self.vertices.last_mut() {
            last.children = self.children.len();
        }
    }

    /// Get a reference to a span by ID.
    ///
    /// # Panics
    ///
    /// Panics if the ID is out of bounds.
    #[inline]
    pub fn get_vertex(&self, id: VertexId) -> &AliveVertex {
        &self.vertices[id.index()]
    }

    /// Get a mutable reference to a span by ID.
    ///
    /// # Panics
    ///
    /// Panics if the ID is out of bounds.
    #[inline]
    pub fn vertex_mut(&mut self, id: VertexId) -> &mut AliveVertex {
        &mut self.vertices[id.index()]
    }

    /// Get a span by ID, returning None if out of bounds.
    #[inline]
    pub fn try_get_vertex(&self, id: VertexId) -> Option<&AliveVertex> {
        self.vertices.get(id.index())
    }

    /// Get a mutable span by ID, returning None if out of bounds.
    #[inline]
    pub fn get_vertex_mut(&mut self, id: VertexId) -> Option<&mut AliveVertex> {
        self.vertices.get_mut(id.index())
    }

    /// Iterate over a span's children.
    ///
    /// This includes both the children in the shared array and any extra
    /// children attached to the span.
    ///
    /// # Arguments
    ///
    /// * `id` - The span whose children to iterate
    ///
    /// # Returns
    ///
    /// An iterator over `(Option<edge>, child_id)` pairs.
    pub fn children(
        &self,
        id: VertexId,
    ) -> impl Iterator<Item = &(Option<SerializedGraphEdge>, VertexId)> {
        let v = &self.vertices[id.index()];
        let start = v.children;
        let end = start + v.n_children;

        self.children[start..end].iter().chain(v.extra.iter())
    }

    /// Get a specific child by index.
    ///
    /// # Arguments
    ///
    /// * `id` - The parent span ID
    /// * `child_index` - Index of the child (0-based)
    ///
    /// # Returns
    ///
    /// The child entry, or None if the index is out of bounds.
    pub fn get_child(
        &self,
        id: VertexId,
        child_index: usize,
    ) -> Option<&(Option<SerializedGraphEdge>, VertexId)> {
        let v = &self.vertices[id.index()];

        if child_index < v.n_children {
            self.children.get(v.children + child_index)
        } else {
            v.extra.get(child_index - v.n_children)
        }
    }

    /// Get the number of children for a span.
    ///
    /// This includes both regular and extra children.
    pub fn child_count(&self, id: VertexId) -> usize {
        let v = &self.vertices[id.index()];
        v.n_children + v.extra.len()
    }

    /// Add an extra child to a span.
    ///
    /// Extra children are stored directly on the span rather than in
    /// the shared array. This is useful when edges are discovered after
    /// initial graph construction.
    ///
    /// # Arguments
    ///
    /// * `id` - The parent span ID
    /// * `edge` - The edge to the child
    /// * `child` - The child span ID
    pub fn add_extra_child(
        &mut self,
        id: VertexId,
        edge: Option<SerializedGraphEdge>,
        child: VertexId,
    ) {
        self.vertices[id.index()].extra.push((edge, child));
    }

    /// Iterate over all vertices.
    pub fn iter_vertices(&self) -> impl Iterator<Item = (VertexId, &AliveVertex)> {
        self.vertices
            .iter()
            .enumerate()
            .map(|(i, v)| (VertexId::new(i), v))
    }

    /// Iterate over all vertices mutably.
    pub fn iter_vertices_mut(&mut self) -> impl Iterator<Item = (VertexId, &mut AliveVertex)> {
        self.vertices
            .iter_mut()
            .enumerate()
            .map(|(i, v)| (VertexId::new(i), v))
    }

    /// Get statistics about this graph.
    pub fn stats(&self) -> GraphStats {
        let mut zombie_count = 0;
        let mut empty_count = 0;

        for v in &self.vertices {
            if v.is_zombie() {
                zombie_count += 1;
            }
            if v.is_empty() {
                empty_count += 1;
            }
        }

        GraphStats {
            vertex_count: self.vertices.len(),
            child_count: self.children.len(),
            total_bytes: self.total_bytes,
            zombie_count,
            empty_vertex_count: empty_count,
        }
    }

    /// Clear the graph, removing all vertices and children.
    ///
    /// This does not deallocate the underlying storage.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.children.clear();
        self.total_bytes = 0;
    }

    /// Reset all traversal state (visited, on_stack, etc.) on vertices.
    ///
    /// This is useful when re-traversing the graph.
    pub fn reset_traversal_state(&mut self) {
        for v in &mut self.vertices {
            v.set_flags(VertexFlags::empty());
            v.index = 0;
            v.lowlink = 0;
            v.scc = 0;
        }
    }
}

impl Default for AliveGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Index<VertexId> for AliveGraph {
    type Output = AliveVertex;

    fn index(&self, id: VertexId) -> &Self::Output {
        &self.vertices[id.index()]
    }
}

impl std::ops::IndexMut<VertexId> for AliveGraph {
    fn index_mut(&mut self, id: VertexId) -> &mut Self::Output {
        &mut self.vertices[id.index()]
    }
}

// ============================================================================
// GRAPH STATISTICS
// ============================================================================

/// Statistics about an alive graph.
///
/// Useful for debugging, logging, and performance analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphStats {
    /// Number of vertices in the graph.
    pub vertex_count: usize,

    /// Number of child entries.
    pub child_count: usize,

    /// Total bytes of content.
    pub total_bytes: usize,

    /// Number of zombie vertices (deleted but with live connections).
    pub zombie_count: usize,

    /// Number of empty (zero-length) vertices.
    pub empty_vertex_count: usize,
}

impl GraphStats {
    /// Check if the graph has any conflicts (zombies).
    pub fn has_conflicts(&self) -> bool {
        self.zombie_count > 0
    }

    /// Average span size in bytes.
    pub fn avg_vertex_size(&self) -> f64 {
        if self.vertex_count == 0 {
            0.0
        } else {
            self.total_bytes as f64 / self.vertex_count as f64
        }
    }

    /// Average children per span.
    pub fn avg_children(&self) -> f64 {
        if self.vertex_count == 0 {
            0.0
        } else {
            self.child_count as f64 / self.vertex_count as f64
        }
    }
}

impl std::fmt::Display for GraphStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} vertices, {} children, {} bytes, {} zombies",
            self.vertex_count, self.child_count, self.total_bytes, self.zombie_count
        )
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangePosition, EdgeFlags, Position};

    // -------------------------------------------------------------------------
    // AliveGraph Basic Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_new() {
        let graph = AliveGraph::new();
        assert!(graph.is_empty());
        assert_eq!(graph.len_vertices(), 0);
        assert_eq!(graph.total_bytes(), 0);
    }

    #[test]
    fn test_graph_default() {
        let graph = AliveGraph::default();
        assert!(graph.is_empty());
    }

    #[test]
    fn test_graph_with_capacity() {
        let graph = AliveGraph::with_capacity(100, 200);
        assert!(graph.is_empty());
    }

    #[test]
    fn test_graph_push_vertex() {
        let mut graph = AliveGraph::new();

        let id = graph.push_vertex(AliveVertex::DUMMY);
        assert_eq!(id, VertexId::DUMMY);
        assert_eq!(graph.len_vertices(), 1);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        let id2 = graph.push_vertex(AliveVertex::new(v));
        assert_eq!(id2, VertexId::new(1));
        assert_eq!(graph.len_vertices(), 2);
        assert_eq!(graph.total_bytes(), 100);
    }

    #[test]
    fn test_graph_vertex_access() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(42),
            ChangePosition::new(0),
            ChangePosition::new(50),
        );
        graph.push_vertex(AliveVertex::new(v));

        let vertex = graph.get_vertex(VertexId::new(1));
        assert_eq!(vertex.node.change, NodeId::new(42));

        let vertex_mut = graph.vertex_mut(VertexId::new(1));
        vertex_mut.mark_visited();
        assert!(graph.get_vertex(VertexId::new(1)).is_visited());
    }

    #[test]
    fn test_graph_try_get_vertex() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        assert!(graph.try_get_vertex(VertexId::DUMMY).is_some());
        assert!(graph.try_get_vertex(VertexId::new(100)).is_none());
    }

    #[test]
    fn test_graph_index_operator() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v));

        assert!(graph[VertexId::DUMMY].is_dummy());
        assert_eq!(graph[VertexId::new(1)].len(), 10);
    }

    #[test]
    fn test_graph_index_mut_operator() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        graph[VertexId::DUMMY].scc = 42;
        assert_eq!(graph[VertexId::DUMMY].scc, 42);
    }

    // -------------------------------------------------------------------------
    // Children Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_push_child() {
        let mut graph = AliveGraph::new();
        graph.push_child(None, VertexId::DUMMY);

        assert_eq!(graph.len_children(), 1);
    }

    #[test]
    fn test_graph_push_child_to_last() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();

        graph.push_child_to_last(None, VertexId::new(2));
        graph.push_child_to_last(None, VertexId::DUMMY);

        assert_eq!(graph.get_vertex(VertexId::new(1)).n_children, 2);
    }

    #[test]
    fn test_graph_children_iterator() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();

        let edge = SerializedGraphEdge::new(
            EdgeFlags::BLOCK,
            Position::new(NodeId::new(2), ChangePosition::new(0)),
            NodeId::new(1),
        );
        graph.push_child_to_last(Some(edge), VertexId::new(2));
        graph.push_child_to_last(None, VertexId::DUMMY);

        let children: Vec<_> = graph.children(VertexId::new(1)).collect();
        assert_eq!(children.len(), 2);
        assert!(children[0].0.is_some());
        assert!(children[1].0.is_none());
    }

    #[test]
    fn test_graph_get_child() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();

        graph.push_child_to_last(None, VertexId::new(10));
        graph.push_child_to_last(None, VertexId::new(11));

        let child0 = graph.get_child(VertexId::new(1), 0);
        assert!(child0.is_some());
        assert_eq!(child0.unwrap().1, VertexId::new(10));

        let child1 = graph.get_child(VertexId::new(1), 1);
        assert_eq!(child1.unwrap().1, VertexId::new(11));

        let child2 = graph.get_child(VertexId::new(1), 2);
        assert!(child2.is_none());
    }

    #[test]
    fn test_graph_add_extra_child() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));

        graph.add_extra_child(VertexId::new(1), None, VertexId::new(5));
        graph.add_extra_child(VertexId::new(1), None, VertexId::new(6));

        assert_eq!(graph.get_vertex(VertexId::new(1)).extra.len(), 2);
        assert_eq!(graph.child_count(VertexId::new(1)), 2);
    }

    #[test]
    fn test_graph_child_count() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();

        graph.push_child_to_last(None, VertexId::new(1));
        graph.push_child_to_last(None, VertexId::new(2));
        graph.add_extra_child(VertexId::new(1), None, VertexId::new(3));

        assert_eq!(graph.child_count(VertexId::new(1)), 3);
    }

    // -------------------------------------------------------------------------
    // Iteration Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_iter_vertices() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(20),
        );
        graph.push_vertex(AliveVertex::new(v1));
        graph.push_vertex(AliveVertex::new(v2));

        let vertices: Vec<_> = graph.iter_vertices().collect();
        assert_eq!(vertices.len(), 3);
        assert_eq!(vertices[0].0, VertexId::DUMMY);
        assert_eq!(vertices[1].0, VertexId::new(1));
        assert_eq!(vertices[2].0, VertexId::new(2));
    }

    #[test]
    fn test_graph_iter_vertices_mut() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(GraphNode::ROOT));

        for (_, vertex) in graph.iter_vertices_mut() {
            vertex.mark_visited();
        }

        assert!(graph.get_vertex(VertexId::DUMMY).is_visited());
        assert!(graph.get_vertex(VertexId::new(1)).is_visited());
    }

    // -------------------------------------------------------------------------
    // Statistics Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_stats_empty() {
        let graph = AliveGraph::new();
        let stats = graph.stats();

        assert_eq!(stats.vertex_count, 0);
        assert_eq!(stats.child_count, 0);
        assert_eq!(stats.total_bytes, 0);
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_graph_stats_with_content() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        graph.push_vertex(AliveVertex::new(v1));

        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(50),
        );
        graph.push_vertex(AliveVertex::new(v2).with_flags(VertexFlags::ZOMBIE));

        let stats = graph.stats();
        assert_eq!(stats.vertex_count, 3);
        assert_eq!(stats.total_bytes, 150);
        assert_eq!(stats.zombie_count, 1);
        assert!(stats.has_conflicts());
    }

    #[test]
    fn test_graph_stats_avg_calculations() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::DUMMY);

        let stats = graph.stats();
        assert!(stats.avg_vertex_size() > 0.0);
        assert!(stats.avg_children() > 0.0);
    }

    #[test]
    fn test_graph_stats_display() {
        let stats = GraphStats {
            vertex_count: 10,
            child_count: 25,
            total_bytes: 1000,
            zombie_count: 2,
            empty_vertex_count: 1,
        };

        let display = stats.to_string();
        assert!(display.contains("10 vertices"));
        assert!(display.contains("25 children"));
        assert!(display.contains("1000 bytes"));
        assert!(display.contains("2 zombies"));
    }

    // -------------------------------------------------------------------------
    // Clear and Reset Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_clear() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        graph.push_vertex(AliveVertex::new(v));

        assert!(!graph.is_empty());

        graph.clear();
        assert!(graph.is_empty());
        assert_eq!(graph.total_bytes(), 0);
    }

    #[test]
    fn test_graph_reset_traversal_state() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));

        graph.vertex_mut(VertexId::new(1)).mark_visited();
        graph.vertex_mut(VertexId::new(1)).scc = 5;

        graph.reset_traversal_state();

        assert!(!graph.get_vertex(VertexId::new(1)).is_visited());
        assert_eq!(graph.get_vertex(VertexId::new(1)).scc, 0);
    }

    // -------------------------------------------------------------------------
    // GraphStats Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_stats_default() {
        let stats = GraphStats::default();
        assert_eq!(stats.vertex_count, 0);
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_graph_stats_avg_empty() {
        let stats = GraphStats::default();
        assert_eq!(stats.avg_vertex_size(), 0.0);
        assert_eq!(stats.avg_children(), 0.0);
    }

    #[test]
    fn test_graph_stats_clone() {
        let stats = GraphStats {
            vertex_count: 5,
            child_count: 10,
            total_bytes: 500,
            zombie_count: 1,
            empty_vertex_count: 2,
        };

        let cloned = stats;
        assert_eq!(stats, cloned);
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_single_vertex() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        assert_eq!(graph.len_vertices(), 1);
        assert_eq!(graph.total_bytes(), 0);
    }

    #[test]
    fn test_graph_children_with_extra() {
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();

        // Regular children
        graph.push_child_to_last(None, VertexId::new(10));

        // Extra children
        graph.add_extra_child(VertexId::new(1), None, VertexId::new(20));
        graph.add_extra_child(VertexId::new(1), None, VertexId::new(21));

        // Iterate all children
        let children: Vec<_> = graph.children(VertexId::new(1)).collect();
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn test_graph_debug() {
        let graph = AliveGraph::new();
        let debug = format!("{:?}", graph);
        assert!(debug.contains("AliveGraph"));
    }
}

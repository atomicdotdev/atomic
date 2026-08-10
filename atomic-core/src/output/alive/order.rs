//! Content ordering with Tarjan's SCC algorithm
//!
//! This module handles ordering vertices for output, detecting conflicts
//! where the graph doesn't have a clear linear ordering. It uses Tarjan's
//! algorithm to find Strongly Connected Components (SCCs), which represent
//! cyclic conflicts.
//!
//! # Overview
//!
//! After retrieving the alive graph, we need to determine the order in which
//! to output vertices. This is straightforward for a DAG, but the graph may
//! contain cycles due to conflicting edits. We handle this by:
//!
//! 1. **Find SCCs**: Use Tarjan's algorithm to identify cycles
//! 2. **Topological sort**: Order SCCs (which are now a DAG)
//! 3. **Detect conflicts**: SCCs with multiple vertices are cyclic conflicts
//! 4. **Build conflict tree**: Nested conflict structure for output
//!
//! # Tarjan's Algorithm
//!
//! Tarjan's algorithm finds all SCCs in a single DFS traversal:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Tarjan's Algorithm                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  For each unvisited span v:                                           │
//! │    1. Set v.index = v.lowlink = current_index++                         │
//! │    2. Push v onto stack                                                 │
//! │    3. For each successor w:                                             │
//! │       - If w unvisited: recurse, then v.lowlink = min(v.lowlink, w)     │
//! │       - If w on stack: v.lowlink = min(v.lowlink, w.index)              │
//! │    4. If v.lowlink == v.index:                                          │
//! │       - Pop stack until v is popped                                     │
//! │       - All popped vertices form one SCC                                │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Conflict Detection
//!
//! - **Single-span SCC**: No conflict, output normally
//! - **Multi-span SCC**: Cyclic conflict, output with conflict markers
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::alive::{AliveGraph, compute_order};
//!
//! // After retrieving graph...
//! let order_result = compute_order(&mut graph)?;
//!
//! // Check for conflicts
//! if order_result.has_conflicts() {
//!     println!("Found {} cyclic conflicts", order_result.cyclic_conflicts);
//! }
//!
//! // Output in SCC order
//! for scc in &order_result.sccs {
//!     if scc.len() > 1 {
//!         // Begin cyclic conflict marker
//!     }
//!     for vertex_id in scc {
//!         // Output span content
//!     }
//!     if scc.len() > 1 {
//!         // End cyclic conflict marker
//!     }
//! }
//! ```

use super::graph::AliveGraph;
use super::vertex::VertexId;

// SCC IDENTIFIER

/// Identifier for a Strongly Connected Component.
///
/// SCCs are numbered in reverse topological order (the first SCC found
/// is the "deepest" in the DAG of SCCs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SccId(pub usize);

impl SccId {
    /// Create a new SCC ID.
    #[inline]
    pub fn new(id: usize) -> Self {
        SccId(id)
    }

    /// Get the underlying index.
    #[inline]
    pub fn index(self) -> usize {
        self.0
    }
}

impl From<usize> for SccId {
    fn from(id: usize) -> Self {
        SccId(id)
    }
}

impl From<SccId> for usize {
    fn from(id: SccId) -> Self {
        id.0
    }
}

impl std::fmt::Display for SccId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SCC{}", self.0)
    }
}

// PATH ELEMENT (FOR CONFLICT TREE)

/// An element in the conflict tree path.
///
/// The conflict tree represents the nested structure of conflicts:
/// - `Scc`: A single SCC (may be trivial with one span)
/// - `Conflict`: Multiple alternative paths (order conflict)
#[derive(Debug, Clone)]
pub enum PathElement {
    /// A strongly connected component.
    ///
    /// If the SCC has multiple vertices, it's a cyclic conflict.
    Scc {
        /// The SCC identifier.
        scc: SccId,
    },

    /// An order conflict with multiple alternative sides.
    ///
    /// Each side is a path of elements that could appear in this position.
    Conflict {
        /// The alternative paths (conflict sides).
        sides: Vec<ConflictPath>,
    },
}

impl PathElement {
    /// Create an SCC path element.
    pub fn scc(id: SccId) -> Self {
        PathElement::Scc { scc: id }
    }

    /// Create a conflict path element.
    pub fn conflict(sides: Vec<ConflictPath>) -> Self {
        PathElement::Conflict { sides }
    }

    /// Check if this is an SCC element.
    pub fn is_scc(&self) -> bool {
        matches!(self, PathElement::Scc { .. })
    }

    /// Check if this is a conflict element.
    pub fn is_conflict(&self) -> bool {
        matches!(self, PathElement::Conflict { .. })
    }

    /// Get the SCC ID if this is an SCC element.
    pub fn scc_id(&self) -> Option<SccId> {
        match self {
            PathElement::Scc { scc } => Some(*scc),
            PathElement::Conflict { .. } => None,
        }
    }
}

// CONFLICT PATH

/// A path through the conflict tree.
///
/// Represents one side of a conflict or the main output path.
#[derive(Debug, Clone, Default)]
pub struct ConflictPath {
    /// The elements in this path, in order.
    pub elements: Vec<PathElement>,
}

impl ConflictPath {
    /// Create a new empty conflict path.
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
        }
    }

    /// Create a path with a single SCC.
    pub fn with_scc(scc: SccId) -> Self {
        Self {
            elements: vec![PathElement::scc(scc)],
        }
    }

    /// Push an element onto the path.
    pub fn push(&mut self, element: PathElement) {
        self.elements.push(element);
    }

    /// Check if the path is empty.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Get the number of elements in the path.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Iterate over elements.
    pub fn iter(&self) -> impl Iterator<Item = &PathElement> {
        self.elements.iter()
    }
}

// CONFLICT TREE

/// The root of the conflict tree.
///
/// This represents the complete output structure, including any nested
/// conflicts that need to be resolved.
#[derive(Debug, Clone)]
pub struct ConflictTree {
    /// The root path of the output.
    pub root: ConflictPath,
}

impl ConflictTree {
    /// Create a new conflict tree with an empty root.
    pub fn new() -> Self {
        Self {
            root: ConflictPath::new(),
        }
    }

    /// Create a conflict tree from a root path.
    pub fn from_path(root: ConflictPath) -> Self {
        Self { root }
    }

    /// Check if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// Count the total number of conflicts in the tree.
    pub fn count_conflicts(&self) -> usize {
        count_conflicts_in_path(&self.root)
    }
}

impl Default for ConflictTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Recursively count conflicts in a path.
fn count_conflicts_in_path(path: &ConflictPath) -> usize {
    let mut count = 0;
    for element in &path.elements {
        match element {
            PathElement::Scc { .. } => {}
            PathElement::Conflict { sides } => {
                count += 1; // This conflict itself
                for side in sides {
                    count += count_conflicts_in_path(side);
                }
            }
        }
    }
    count
}

// ORDER RESULT

/// Result of computing the output order.
#[derive(Debug)]
pub struct OrderResult {
    /// SCCs in reverse topological order.
    ///
    /// Each SCC is a vector of span IDs. Single-element SCCs are
    /// trivial (no cycle), multi-element SCCs are cyclic conflicts.
    pub sccs: Vec<Vec<VertexId>>,

    /// The conflict tree for output.
    pub conflict_tree: ConflictTree,

    /// Number of cyclic conflicts (SCCs with > 1 span).
    pub cyclic_conflicts: usize,

    /// Forward edges that could be removed to simplify the graph.
    pub forward_edges: Vec<(VertexId, VertexId)>,
}

impl OrderResult {
    /// Create a new order result.
    fn new() -> Self {
        Self {
            sccs: Vec::new(),
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        }
    }

    /// Check if there are any conflicts.
    pub fn has_conflicts(&self) -> bool {
        self.cyclic_conflicts > 0 || self.conflict_tree.count_conflicts() > 0
    }

    /// Get the total number of vertices across all SCCs.
    pub fn total_vertices(&self) -> usize {
        self.sccs.iter().map(|scc| scc.len()).sum()
    }

    /// Get the number of SCCs.
    pub fn num_sccs(&self) -> usize {
        self.sccs.len()
    }

    /// Verify that the SCCs form an exact partition of the graph's alive
    /// vertices.
    ///
    /// After [`compute_order`], every non-DUMMY vertex
    /// (`VertexId(1)..VertexId(vertex_count)`) must appear in exactly one
    /// SCC exactly once. A violation means the ordering stage has lost or
    /// duplicated a vertex, which downstream corrupts the emitted file
    /// (duplicated tail, relocated line) while reporting success. This is
    /// the structural guard for that entire class of bugs.
    ///
    /// `vertex_count` is the graph's [`AliveGraph::len_vertices`], i.e. the
    /// DUMMY sentinel at index 0 plus all alive vertices.
    ///
    /// Cheap (O(total vertices) with a single bitmap), so property tests and
    /// release builds can call it explicitly; [`compute_order`] runs it under
    /// `debug_assert!`.
    pub fn validate_partition(&self, vertex_count: usize) -> Result<(), OrderInvariantError> {
        // Index 0 is the DUMMY sentinel and must never appear in an SCC.
        // Alive vertices are 1..vertex_count.
        let mut seen = vec![false; vertex_count];
        for scc in &self.sccs {
            for &vid in scc {
                let idx = vid.index();
                if idx == 0 {
                    return Err(OrderInvariantError::DummyInScc);
                }
                if idx >= vertex_count {
                    return Err(OrderInvariantError::OutOfRange {
                        vertex: vid,
                        vertex_count,
                    });
                }
                if seen[idx] {
                    return Err(OrderInvariantError::Duplicate { vertex: vid });
                }
                seen[idx] = true;
            }
        }
        // Every alive vertex must have been covered.
        for (idx, covered) in seen.iter().enumerate().skip(1) {
            if !*covered {
                return Err(OrderInvariantError::Missing {
                    vertex: VertexId::new(idx),
                });
            }
        }
        Ok(())
    }
}

/// A violation of the SCC-partition invariant detected by
/// [`OrderResult::validate_partition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderInvariantError {
    /// The DUMMY sentinel (index 0) appeared in an SCC.
    DummyInScc,
    /// A vertex index was outside `1..vertex_count`.
    OutOfRange {
        /// The offending vertex.
        vertex: VertexId,
        /// The graph's vertex count.
        vertex_count: usize,
    },
    /// A vertex appeared in more than one SCC (or twice in one).
    Duplicate {
        /// The vertex emitted more than once.
        vertex: VertexId,
    },
    /// An alive vertex never appeared in any SCC.
    Missing {
        /// The vertex that was dropped from the ordering.
        vertex: VertexId,
    },
}

impl std::fmt::Display for OrderInvariantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderInvariantError::DummyInScc => {
                write!(f, "DUMMY sentinel (VertexId(0)) appeared in an SCC")
            }
            OrderInvariantError::OutOfRange {
                vertex,
                vertex_count,
            } => write!(
                f,
                "{:?} is out of range for a graph with {} vertices",
                vertex, vertex_count
            ),
            OrderInvariantError::Duplicate { vertex } => {
                write!(f, "{:?} appeared in the ordering more than once", vertex)
            }
            OrderInvariantError::Missing { vertex } => {
                write!(f, "{:?} was dropped from the ordering", vertex)
            }
        }
    }
}

impl std::error::Error for OrderInvariantError {}

// TARJAN'S ALGORITHM

/// Compute the output order using Tarjan's SCC algorithm.
///
/// This function:
/// 1. Runs Tarjan's algorithm to find all SCCs
/// 2. Builds the conflict tree
/// 3. Identifies forward edges
///
/// # Arguments
///
/// * `graph` - The alive graph (will be modified to set SCC indices)
///
/// # Returns
///
/// An `OrderResult` containing SCCs and conflict information.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::{AliveGraph, compute_order};
///
/// let mut graph = /* retrieve graph */;
/// let order = compute_order(&mut graph);
///
/// println!("Found {} SCCs", order.num_sccs());
/// ```
pub fn compute_order(graph: &mut AliveGraph) -> OrderResult {
    let mut result = OrderResult::new();

    if graph.is_empty() {
        return result;
    }

    let mut state = TarjanState::new(graph.len_vertices());

    // Run Tarjan's algorithm starting from span 1 (root)
    // Skip span 0 which is DUMMY
    for i in 1..graph.len_vertices() {
        let vid = VertexId::new(i);
        if !graph.get_vertex(vid).is_visited() {
            tarjan_visit(graph, vid, &mut state, &mut result);
        }
    }

    // Count cyclic conflicts
    result.cyclic_conflicts = result.sccs.iter().filter(|scc| scc.len() > 1).count();

    // Build a simple conflict tree (can be enhanced later for nested conflicts)
    build_conflict_tree(graph, &mut result);

    // Structural invariant: the SCCs must partition the alive vertices. A
    // violation here is the root of the silent duplication/omission class of
    // bugs, so fail loudly (in debug/test builds) at the point of origin
    // rather than letting corruption reach disk.
    debug_assert!(
        result.validate_partition(graph.len_vertices()).is_ok(),
        "compute_order did not produce a vertex partition: {:?}",
        result.validate_partition(graph.len_vertices())
    );

    result
}

/// Internal state for Tarjan's algorithm.
struct TarjanState {
    /// Current index for DFS ordering.
    index: usize,
    /// Stack of vertices being processed.
    stack: Vec<VertexId>,
}

impl TarjanState {
    fn new(capacity: usize) -> Self {
        Self {
            index: 0,
            stack: Vec::with_capacity(capacity),
        }
    }
}

/// Frame for the iterative Tarjan work stack.
///
/// Each frame represents a vertex being processed and tracks how far
/// through its children list we've progressed.
struct TarjanFrame {
    v: VertexId,
    children: Vec<VertexId>,
    child_idx: usize,
}

/// Iterative Tarjan visit function.
///
/// Equivalent to the classic recursive Tarjan's SCC algorithm, but uses
/// an explicit work stack instead of the call stack.  This avoids stack
/// overflow for files with tens of thousands of vertices.
fn tarjan_visit(
    graph: &mut AliveGraph,
    start: VertexId,
    state: &mut TarjanState,
    result: &mut OrderResult,
) {
    let mut work: Vec<TarjanFrame> = Vec::new();

    // Initialize the start vertex
    {
        let vertex = graph.vertex_mut(start);
        vertex.index = state.index;
        vertex.lowlink = state.index;
        vertex.mark_visited();
        vertex.push_stack();
    }
    state.index += 1;
    state.stack.push(start);

    let children: Vec<VertexId> = graph
        .children(start)
        .map(|(_, child)| *child)
        .filter(|c| !c.is_dummy())
        .collect();
    work.push(TarjanFrame {
        v: start,
        children,
        child_idx: 0,
    });

    while let Some(frame) = work.last_mut() {
        if frame.child_idx < frame.children.len() {
            let w = frame.children[frame.child_idx];
            frame.child_idx += 1;

            let w_visited = graph.get_vertex(w).is_visited();
            let w_on_stack = graph.get_vertex(w).is_on_stack();

            if !w_visited {
                // Initialize w and push a new frame (replaces recursive call)
                {
                    let vertex = graph.vertex_mut(w);
                    vertex.index = state.index;
                    vertex.lowlink = state.index;
                    vertex.mark_visited();
                    vertex.push_stack();
                }
                state.index += 1;
                state.stack.push(w);

                let w_children: Vec<VertexId> = graph
                    .children(w)
                    .map(|(_, child)| *child)
                    .filter(|c| !c.is_dummy())
                    .collect();
                work.push(TarjanFrame {
                    v: w,
                    children: w_children,
                    child_idx: 0,
                });
            } else if w_on_stack {
                // Successor is on stack, hence in current SCC
                let w_index = graph.get_vertex(w).index;
                let v = frame.v;
                let v_vertex = graph.vertex_mut(v);
                v_vertex.lowlink = v_vertex.lowlink.min(w_index);
            }
        } else {
            // All children processed — equivalent to the return from
            // the recursive call.  Pop this frame and propagate lowlink
            // to the parent.
            let finished = work.pop().unwrap();
            let v = finished.v;

            // Propagate lowlink to parent frame
            if let Some(parent) = work.last() {
                let v_lowlink = graph.get_vertex(v).lowlink;
                let parent_v = parent.v;
                let pv = graph.vertex_mut(parent_v);
                pv.lowlink = pv.lowlink.min(v_lowlink);
            }

            // If v is a root node, pop the SCC stack and emit the SCC
            let v_index = graph.get_vertex(v).index;
            let v_lowlink = graph.get_vertex(v).lowlink;

            if v_lowlink == v_index {
                let mut scc = Vec::new();
                let scc_id = SccId::new(result.sccs.len());

                loop {
                    let w = state.stack.pop().expect("stack should not be empty");
                    graph.vertex_mut(w).pop_stack();
                    graph.vertex_mut(w).scc = scc_id.index();
                    scc.push(w);

                    if w == v {
                        break;
                    }
                }

                result.sccs.push(scc);
            }
        }
    }
}

/// Build the conflict tree from SCCs.
///
/// This creates a simple linear conflict tree. More sophisticated
/// conflict detection (order conflicts) would enhance this.
fn build_conflict_tree(graph: &AliveGraph, result: &mut OrderResult) {
    let mut path = ConflictPath::new();

    // Add SCCs in reverse order (they come out in reverse topological order)
    for (i, scc) in result.sccs.iter().enumerate().rev() {
        let scc_id = SccId::new(i);

        // For multi-span SCCs, we could create nested conflict structures
        // For now, just add them as SCCs
        path.push(PathElement::scc(scc_id));

        // Track if any span in this SCC is a zombie
        let _has_zombie = scc.iter().any(|&vid| {
            graph
                .try_get_vertex(vid)
                .map(|v| v.is_zombie())
                .unwrap_or(false)
        });
    }

    result.conflict_tree = ConflictTree::from_path(path);
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::alive::vertex::AliveVertex;
    use crate::types::{ChangePosition, GraphNode, NodeId};

    // -------------------------------------------------------------------------
    // SccId Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_scc_id_new() {
        let id = SccId::new(42);
        assert_eq!(id.index(), 42);
    }

    #[test]
    fn test_scc_id_from_usize() {
        let id: SccId = 100.into();
        assert_eq!(id.index(), 100);
    }

    #[test]
    fn test_scc_id_into_usize() {
        let id = SccId::new(50);
        let index: usize = id.into();
        assert_eq!(index, 50);
    }

    #[test]
    fn test_scc_id_display() {
        let id = SccId::new(7);
        assert_eq!(id.to_string(), "SCC7");
    }

    #[test]
    fn test_scc_id_ordering() {
        let s1 = SccId::new(1);
        let s2 = SccId::new(2);
        assert!(s1 < s2);
    }

    #[test]
    fn test_scc_id_equality() {
        let s1 = SccId::new(5);
        let s2 = SccId::new(5);
        let s3 = SccId::new(6);
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn test_scc_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SccId::new(1));
        set.insert(SccId::new(2));
        set.insert(SccId::new(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    // -------------------------------------------------------------------------
    // PathElement Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_path_element_scc() {
        let elem = PathElement::scc(SccId::new(5));
        assert!(elem.is_scc());
        assert!(!elem.is_conflict());
        assert_eq!(elem.scc_id(), Some(SccId::new(5)));
    }

    #[test]
    fn test_path_element_conflict() {
        let sides = vec![ConflictPath::new(), ConflictPath::new()];
        let elem = PathElement::conflict(sides);
        assert!(!elem.is_scc());
        assert!(elem.is_conflict());
        assert_eq!(elem.scc_id(), None);
    }

    #[test]
    fn test_path_element_debug() {
        let elem = PathElement::scc(SccId::new(1));
        let debug = format!("{:?}", elem);
        assert!(debug.contains("Scc"));
    }

    #[test]
    fn test_path_element_clone() {
        let elem = PathElement::scc(SccId::new(3));
        let cloned = elem.clone();
        assert_eq!(elem.scc_id(), cloned.scc_id());
    }

    // -------------------------------------------------------------------------
    // ConflictPath Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_conflict_path_new() {
        let path = ConflictPath::new();
        assert!(path.is_empty());
        assert_eq!(path.len(), 0);
    }

    #[test]
    fn test_conflict_path_default() {
        let path = ConflictPath::default();
        assert!(path.is_empty());
    }

    #[test]
    fn test_conflict_path_with_scc() {
        let path = ConflictPath::with_scc(SccId::new(1));
        assert!(!path.is_empty());
        assert_eq!(path.len(), 1);
    }

    #[test]
    fn test_conflict_path_push() {
        let mut path = ConflictPath::new();
        path.push(PathElement::scc(SccId::new(1)));
        path.push(PathElement::scc(SccId::new(2)));
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn test_conflict_path_iter() {
        let mut path = ConflictPath::new();
        path.push(PathElement::scc(SccId::new(0)));
        path.push(PathElement::scc(SccId::new(1)));

        let elements: Vec<_> = path.iter().collect();
        assert_eq!(elements.len(), 2);
    }

    // -------------------------------------------------------------------------
    // ConflictTree Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_conflict_tree_new() {
        let tree = ConflictTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.count_conflicts(), 0);
    }

    #[test]
    fn test_conflict_tree_default() {
        let tree = ConflictTree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn test_conflict_tree_from_path() {
        let path = ConflictPath::with_scc(SccId::new(1));
        let tree = ConflictTree::from_path(path);
        assert!(!tree.is_empty());
    }

    #[test]
    fn test_conflict_tree_count_conflicts() {
        let mut path = ConflictPath::new();
        path.push(PathElement::scc(SccId::new(0)));

        let sides = vec![ConflictPath::new(), ConflictPath::new()];
        path.push(PathElement::conflict(sides));

        let tree = ConflictTree::from_path(path);
        assert_eq!(tree.count_conflicts(), 1);
    }

    #[test]
    fn test_conflict_tree_nested_conflicts() {
        let mut inner_path = ConflictPath::new();
        let inner_sides = vec![ConflictPath::new()];
        inner_path.push(PathElement::conflict(inner_sides));

        let outer_sides = vec![inner_path, ConflictPath::new()];

        let mut root = ConflictPath::new();
        root.push(PathElement::conflict(outer_sides));

        let tree = ConflictTree::from_path(root);
        assert_eq!(tree.count_conflicts(), 2); // Outer + inner
    }

    // -------------------------------------------------------------------------
    // OrderResult Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_order_result_new() {
        let result = OrderResult::new();
        assert!(result.sccs.is_empty());
        assert!(!result.has_conflicts());
        assert_eq!(result.total_vertices(), 0);
        assert_eq!(result.num_sccs(), 0);
    }

    #[test]
    fn test_order_result_has_conflicts_cyclic() {
        let mut result = OrderResult::new();
        result.cyclic_conflicts = 1;
        assert!(result.has_conflicts());
    }

    #[test]
    fn test_order_result_total_vertices() {
        let mut result = OrderResult::new();
        result.sccs.push(vec![VertexId::new(1), VertexId::new(2)]);
        result.sccs.push(vec![VertexId::new(3)]);
        assert_eq!(result.total_vertices(), 3);
    }

    #[test]
    fn test_order_result_num_sccs() {
        let mut result = OrderResult::new();
        result.sccs.push(vec![VertexId::new(1)]);
        result.sccs.push(vec![VertexId::new(2)]);
        assert_eq!(result.num_sccs(), 2);
    }

    #[test]
    fn test_order_result_debug() {
        let result = OrderResult::new();
        let debug = format!("{:?}", result);
        assert!(debug.contains("OrderResult"));
    }

    // -------------------------------------------------------------------------
    // Partition-invariant Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_validate_partition_accepts_exact_cover() {
        // 3 alive vertices (indices 1..=3) in a graph of size 4 (DUMMY + 3),
        // spread across a cyclic SCC and a trivial SCC — still a partition.
        let result = OrderResult {
            sccs: vec![vec![VertexId(1), VertexId(2)], vec![VertexId(3)]],
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 1,
            forward_edges: Vec::new(),
        };
        assert_eq!(result.validate_partition(4), Ok(()));
    }

    #[test]
    fn test_validate_partition_rejects_duplicate() {
        // VertexId(1) appears in two SCCs — the duplication signature.
        let result = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(1), VertexId(2)]],
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 1,
            forward_edges: Vec::new(),
        };
        assert_eq!(
            result.validate_partition(3),
            Err(OrderInvariantError::Duplicate {
                vertex: VertexId(1)
            })
        );
    }

    #[test]
    fn test_validate_partition_rejects_missing() {
        // VertexId(2) is alive (graph size 3) but never appears — dropped.
        let result = OrderResult {
            sccs: vec![vec![VertexId(1)]],
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };
        assert_eq!(
            result.validate_partition(3),
            Err(OrderInvariantError::Missing {
                vertex: VertexId(2)
            })
        );
    }

    #[test]
    fn test_validate_partition_rejects_dummy() {
        let result = OrderResult {
            sccs: vec![vec![VertexId(0)]],
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };
        assert_eq!(
            result.validate_partition(1),
            Err(OrderInvariantError::DummyInScc)
        );
    }

    #[test]
    fn test_validate_partition_rejects_out_of_range() {
        let result = OrderResult {
            sccs: vec![vec![VertexId(999)]],
            conflict_tree: ConflictTree::new(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };
        assert_eq!(
            result.validate_partition(2),
            Err(OrderInvariantError::OutOfRange {
                vertex: VertexId(999),
                vertex_count: 2,
            })
        );
    }

    #[test]
    fn test_compute_order_produces_partition_on_linear_chain() {
        // A real graph run through compute_order must satisfy the invariant.
        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);

        for i in 1..=3u64 {
            let v = GraphNode::new(
                NodeId::new(i),
                ChangePosition::new(0),
                ChangePosition::new(10),
            );
            graph.push_vertex(AliveVertex::new(v));
            graph.set_last_children_start();
            let next = if i < 3 {
                VertexId::new((i + 1) as usize)
            } else {
                VertexId::DUMMY
            };
            graph.push_child_to_last(None, next);
        }

        let result = compute_order(&mut graph);
        assert_eq!(result.validate_partition(graph.len_vertices()), Ok(()));
    }

    // -------------------------------------------------------------------------
    // Tarjan Algorithm Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_compute_order_empty_graph() {
        let mut graph = AliveGraph::new();
        let result = compute_order(&mut graph);

        assert!(result.sccs.is_empty());
        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_compute_order_single_vertex() {
        let mut graph = AliveGraph::new();

        // Add dummy
        graph.push_vertex(AliveVertex::DUMMY);

        // Add single span
        let v = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::DUMMY);

        let result = compute_order(&mut graph);

        assert_eq!(result.num_sccs(), 1);
        assert_eq!(result.cyclic_conflicts, 0);
    }

    #[test]
    fn test_compute_order_linear_chain() {
        let mut graph = AliveGraph::new();

        // Add dummy
        graph.push_vertex(AliveVertex::DUMMY);

        // V1 -> V2 -> V3 (linear chain)
        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v1));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::new(2));
        graph.push_child_to_last(None, VertexId::DUMMY);

        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v2));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::new(3));
        graph.push_child_to_last(None, VertexId::DUMMY);

        let v3 = GraphNode::new(
            NodeId::new(3),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        graph.push_vertex(AliveVertex::new(v3));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::DUMMY);

        let result = compute_order(&mut graph);

        // Each span should be its own SCC (no cycles)
        assert_eq!(result.num_sccs(), 3);
        assert_eq!(result.cyclic_conflicts, 0);
        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_compute_order_sets_scc_on_vertices() {
        let mut graph = AliveGraph::new();

        graph.push_vertex(AliveVertex::DUMMY);

        let v = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(v));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::DUMMY);

        let _ = compute_order(&mut graph);

        // Span should have SCC set
        assert!(graph.get_vertex(VertexId::new(1)).is_visited());
    }

    #[test]
    fn test_compute_order_marks_vertices_visited() {
        let mut graph = AliveGraph::new();

        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(GraphNode::ROOT));
        graph.set_last_children_start();
        graph.push_child_to_last(None, VertexId::DUMMY);

        let _ = compute_order(&mut graph);

        assert!(graph.get_vertex(VertexId::new(1)).is_visited());
        assert!(!graph.get_vertex(VertexId::new(1)).is_on_stack());
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_scc_id_zero() {
        let id = SccId::new(0);
        assert_eq!(id.index(), 0);
    }

    #[test]
    fn test_scc_id_max() {
        let id = SccId::new(usize::MAX);
        assert_eq!(id.index(), usize::MAX);
    }

    #[test]
    fn test_empty_conflict_sides() {
        let sides: Vec<ConflictPath> = vec![];
        let elem = PathElement::conflict(sides);
        assert!(elem.is_conflict());
    }

    #[test]
    fn test_path_with_only_conflicts() {
        let mut path = ConflictPath::new();
        let sides = vec![ConflictPath::new()];
        path.push(PathElement::conflict(sides));

        assert_eq!(path.len(), 1);
    }

    #[test]
    fn test_order_result_forward_edges() {
        let mut result = OrderResult::new();
        result
            .forward_edges
            .push((VertexId::new(1), VertexId::new(3)));

        assert_eq!(result.forward_edges.len(), 1);
    }

    // -------------------------------------------------------------------------
    // TarjanState Tests (Internal)
    // -------------------------------------------------------------------------

    #[test]
    fn test_tarjan_state_new() {
        let state = TarjanState::new(100);
        assert_eq!(state.index, 0);
        assert!(state.stack.is_empty());
    }

    // -------------------------------------------------------------------------
    // count_conflicts_in_path Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_count_conflicts_empty_path() {
        let path = ConflictPath::new();
        assert_eq!(count_conflicts_in_path(&path), 0);
    }

    #[test]
    fn test_count_conflicts_scc_only() {
        let path = ConflictPath::with_scc(SccId::new(1));
        assert_eq!(count_conflicts_in_path(&path), 0);
    }

    #[test]
    fn test_count_conflicts_single_conflict() {
        let mut path = ConflictPath::new();
        let sides = vec![ConflictPath::new(), ConflictPath::new()];
        path.push(PathElement::conflict(sides));

        assert_eq!(count_conflicts_in_path(&path), 1);
    }
}

//! Atomic graph operations (atoms)
//!
//! Atoms are the primitive operations that modify the repository graph.
//! Every change is composed of one or more atoms, which are combined into
//! hunks for higher-level semantic meaning.
//!
//! # Atom Types
//!
//! There are two fundamental atom types:
//!
//! - **Insertion**: Insert new content into the graph
//! - **EdgeUpdate**: Modify existing edges (mark deleted, change flags, etc.)
//!
//! # Graph Model
//!
//! The repository graph consists of:
//! - **Vertices**: Contiguous chunks of content (identified by change + position range)
//! - **Edges**: Ordered relationships between vertices (with flags like DELETED, FOLDER, etc.)
//!
//! Atoms operate on this graph:
//! - `Insertion` creates a new span and connects it to existing vertices via context
//! - `EdgeUpdate` modifies the flags on existing edges (e.g., marking content as deleted)
//!
//! # Context
//!
//! When inserting new content, we specify **context** - the vertices that should
//! come before (predecessors) and after (successors) the new content. This allows
//! Atomic to correctly position the content even when multiple independent changes
//! are being merged.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::{Atom, Insertion, EdgeUpdate, NewEdge};
//! use atomic_core::{EdgeFlags, Position, Span, Hash, ChangePosition};
//!
//! // Insert new content after position X
//! let insert = Insertion {
//!     predecessors: vec![position_x],
//!     successors: vec![],
//!     flag: EdgeFlags::BLOCK,
//!     start: ChangePosition::new(0),
//!     end: ChangePosition::new(10),
//!     inode: file_inode,
//! };
//!
//! // Mark existing content as deleted
//! let delete = EdgeUpdate {
//!     edges: vec![NewEdge {
//!         previous: EdgeFlags::BLOCK,
//!         flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
//!         from: start_pos,
//!         to: end_vertex,
//!         introduced_by: original_change,
//!     }],
//!     inode: file_inode,
//! };
//! ```

#[allow(unused_imports)]
use crate::{ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position};
use serde::{Deserialize, Serialize};
use std::fmt;

/// An atomic graph operation.
///
/// This enum represents the two fundamental ways to modify the graph:
/// - Insert new content (`Insertion`)
/// - Modify existing edges (`EdgeUpdate`)
///
/// # Type Parameter
///
/// - `H`: The change identifier type. Use `Hash` for external (serialized)
///   representation, or `Option<Hash>` when the change being created
///   doesn't yet have a hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Atom<H> {
    /// Insert new content into the graph
    Insertion(Insertion<H>),
    /// Modify existing edges
    EdgeUpdate(EdgeUpdate<H>),
}

impl<H> Atom<H> {
    /// Check if this is a Insertion atom.
    #[inline]
    pub fn is_new_vertex(&self) -> bool {
        matches!(self, Atom::Insertion(_))
    }

    /// Check if this is an EdgeUpdate atom.
    #[inline]
    pub fn is_edge_map(&self) -> bool {
        matches!(self, Atom::EdgeUpdate(_))
    }

    /// Get a reference to the inner Insertion, if this is one.
    pub fn as_new_vertex(&self) -> Option<&Insertion<H>> {
        match self {
            Atom::Insertion(v) => Some(v),
            _ => None,
        }
    }

    /// Get a mutable reference to the inner Insertion, if this is one.
    pub fn as_new_vertex_mut(&mut self) -> Option<&mut Insertion<H>> {
        match self {
            Atom::Insertion(v) => Some(v),
            _ => None,
        }
    }

    /// Get a reference to the inner EdgeUpdate, if this is one.
    pub fn as_edge_map(&self) -> Option<&EdgeUpdate<H>> {
        match self {
            Atom::EdgeUpdate(e) => Some(e),
            _ => None,
        }
    }

    /// Get a mutable reference to the inner EdgeUpdate, if this is one.
    pub fn as_edge_map_mut(&mut self) -> Option<&mut EdgeUpdate<H>> {
        match self {
            Atom::EdgeUpdate(e) => Some(e),
            _ => None,
        }
    }

    /// Get the inode (file reference) for this atom.
    pub fn inode(&self) -> &Position<H> {
        match self {
            Atom::Insertion(v) => &v.inode,
            Atom::EdgeUpdate(e) => &e.inode,
        }
    }
}

impl<H: Clone> Atom<H> {
    /// Convert this atom to use `Option<H>` for the change references.
    ///
    /// This is useful when building a change that references itself
    /// (using None for self-references).
    pub fn to_option(&self) -> Atom<Option<H>> {
        match self {
            Atom::Insertion(v) => Atom::Insertion(v.to_option()),
            Atom::EdgeUpdate(e) => Atom::EdgeUpdate(e.to_option()),
        }
    }
}

impl<H> From<Insertion<H>> for Atom<H> {
    fn from(v: Insertion<H>) -> Self {
        Atom::Insertion(v)
    }
}

impl<H> From<EdgeUpdate<H>> for Atom<H> {
    fn from(e: EdgeUpdate<H>) -> Self {
        Atom::EdgeUpdate(e)
    }
}

/// Insert new content into the graph.
///
/// A Insertion creates a new span (chunk of content) and connects it
/// to the graph via context edges. The context specifies where in the
/// graph this content should appear.
///
/// # Fields
///
/// - `predecessors`: Vertices that should come **before** this content
/// - `successors`: Vertices that should come **after** this content
/// - `flag`: Edge flags for the new edges (typically `BLOCK`)
/// - `start`, `end`: Position range in the change's content blob
/// - `inode`: The file this content belongs to
///
/// # Content
///
/// The actual bytes are stored in the change's `contents` blob. The
/// `start` and `end` fields specify the byte range within that blob.
/// This allows efficient storage when a single change modifies multiple
/// files or locations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insertion<H> {
    /// Vertices that should come before this new content.
    ///
    /// During application, edges are created FROM each predecessors
    /// TO the new span.
    pub predecessors: Vec<Position<H>>,

    /// Vertices that should come after this new content.
    ///
    /// During application, edges are created FROM the new span
    /// TO each successors.
    pub successors: Vec<Position<H>>,

    /// Flags for the new edges.
    ///
    /// Typically `EdgeFlags::BLOCK` for file content, or
    /// `EdgeFlags::FOLDER` for directory structure.
    pub flag: EdgeFlags,

    /// Start offset in the change's content blob (inclusive).
    pub start: ChangePosition,

    /// End offset in the change's content blob (exclusive).
    pub end: ChangePosition,

    /// The file (inode) this span belongs to.
    ///
    /// This allows efficient lookup of which file a span is part of.
    pub inode: Position<H>,
}

impl<H> Insertion<H> {
    /// Get the length of the content in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Check if this span has zero length.
    ///
    /// Zero-length vertices are used for structural nodes like
    /// file roots and directory entries.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Check if this span has any up context.
    #[inline]
    pub fn has_predecessors(&self) -> bool {
        !self.predecessors.is_empty()
    }

    /// Check if this span has any down context.
    #[inline]
    pub fn has_successors(&self) -> bool {
        !self.successors.is_empty()
    }
}

impl<H: Clone> Insertion<H> {
    /// Convert to use `Option<H>` for change references.
    pub fn to_option(&self) -> Insertion<Option<H>> {
        Insertion {
            predecessors: self
                .predecessors
                .iter()
                .map(|p| p.clone().to_option())
                .collect(),
            successors: self
                .successors
                .iter()
                .map(|p| p.clone().to_option())
                .collect(),
            flag: self.flag,
            start: self.start,
            end: self.end,
            inode: self.inode.clone().to_option(),
        }
    }
}

impl<H: fmt::Debug> fmt::Display for Insertion<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Insertion[{}..{}] ({} up, {} down)",
            self.start.get(),
            self.end.get(),
            self.predecessors.len(),
            self.successors.len()
        )
    }
}

// Extension trait for Position to add to_option method
trait PositionExt<H> {
    fn to_option(self) -> Position<Option<H>>;
}

impl<H> PositionExt<H> for Position<H> {
    fn to_option(self) -> Position<Option<H>> {
        Position {
            change: Some(self.change),
            pos: self.pos,
        }
    }
}

/// Modify existing edges in the graph.
///
/// An EdgeUpdate contains a list of edge modifications. Each modification
/// specifies:
/// - The edge to modify (from position → to span)
/// - The previous flags (what the edge currently has)
/// - The new flags (what the edge should become)
///
/// This is primarily used to:
/// - Mark content as deleted (add DELETED flag)
/// - Resolve conflicts (modify pseudo-edges)
/// - Update folder structure
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeUpdate<H> {
    /// The edge modifications to apply.
    pub edges: Vec<NewEdge<H>>,

    /// The file (inode) these edges belong to.
    pub inode: Position<H>,
}

impl<H> EdgeUpdate<H> {
    /// Create a new empty EdgeUpdate for a given inode.
    pub fn new(inode: Position<H>) -> Self {
        Self {
            edges: Vec::new(),
            inode,
        }
    }

    /// Check if this EdgeUpdate has no edge modifications.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Get the number of edge modifications.
    #[inline]
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Add an edge modification.
    pub fn push(&mut self, edge: NewEdge<H>) {
        self.edges.push(edge);
    }
}

impl<H: Clone> EdgeUpdate<H> {
    /// Convert to use `Option<H>` for change references.
    pub fn to_option(&self) -> EdgeUpdate<Option<H>> {
        EdgeUpdate {
            edges: self.edges.iter().map(|e| e.to_option()).collect(),
            inode: self.inode.clone().to_option(),
        }
    }

    /// Concatenate another EdgeUpdate's edges into this one.
    ///
    /// Note: The inode must match (this is the caller's responsibility).
    pub fn concat(&mut self, other: &EdgeUpdate<H>) {
        self.edges.extend(other.edges.iter().cloned());
    }
}

impl<H> fmt::Display for EdgeUpdate<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EdgeUpdate({} edges)", self.edges.len())
    }
}

/// A single edge modification.
///
/// This specifies how to modify one edge in the graph:
/// - `from`: The source position of the edge
/// - `to`: The destination span of the edge
/// - `previous`: The flags the edge currently has
/// - `flag`: The flags the edge should have after modification
/// - `introduced_by`: The change that originally introduced this edge
///
/// # Flag Semantics
///
/// - `previous == flag`: No change (edge is as expected)
/// - `previous != flag`: Modify the edge's flags
/// - `flag` has DELETED added: Mark content as deleted
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEdge<H> {
    /// The flags the edge currently has.
    ///
    /// This is used to verify the edge exists in the expected state
    /// before modifying it.
    pub previous: EdgeFlags,

    /// The flags the edge should have after this modification.
    ///
    /// Common pattern: add DELETED flag to mark content as deleted.
    pub flag: EdgeFlags,

    /// The source position of the edge.
    ///
    /// This is a position within a span. If the edge starts in the
    /// middle of a span, the span will be split during application.
    pub from: Position<H>,

    /// The destination span of the edge.
    ///
    /// The edge points from `from` to the start of this span.
    pub to: GraphNode<H>,

    /// The change that originally introduced this edge.
    ///
    /// This is used for dependency tracking - if we're modifying an edge,
    /// we depend on the change that created it.
    pub introduced_by: H,
}

impl<H: Clone> NewEdge<H> {
    /// Create the reverse edge modification.
    ///
    /// This creates an edge that undoes this modification:
    /// - Swaps `previous` and `flag`
    /// - Uses the given `introduced_by` for the reverse
    ///
    /// This is used when computing the inverse of a change.
    pub fn reverse(&self, introduced_by: H) -> Self {
        NewEdge {
            previous: self.flag,
            flag: self.previous,
            from: self.from.clone(),
            to: self.to.clone(),
            introduced_by,
        }
    }

    /// Convert to use `Option<H>` for change references.
    pub fn to_option(&self) -> NewEdge<Option<H>> {
        NewEdge {
            previous: self.previous,
            flag: self.flag,
            from: self.from.clone().to_option(),
            to: GraphNode {
                change: Some(self.to.change.clone()),
                start: self.to.start,
                end: self.to.end,
            },
            introduced_by: Some(self.introduced_by.clone()),
        }
    }

    /// Check if this edge modification adds the DELETED flag.
    #[inline]
    pub fn is_deletion(&self) -> bool {
        !self.previous.contains(EdgeFlags::DELETED) && self.flag.contains(EdgeFlags::DELETED)
    }

    /// Check if this edge modification removes the DELETED flag.
    #[inline]
    pub fn is_undeletion(&self) -> bool {
        self.previous.contains(EdgeFlags::DELETED) && !self.flag.contains(EdgeFlags::DELETED)
    }

    /// Check if the flags actually change.
    #[inline]
    pub fn is_noop(&self) -> bool {
        self.previous == self.flag
    }
}

impl<H: fmt::Debug> fmt::Display for NewEdge<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Edge({:?} -> {:?}): {} → {}",
            self.from, self.to, self.previous, self.flag
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeId;

    // Helper to create test positions
    #[allow(dead_code)]
    fn test_position(change: u64, pos: u64) -> Position<NodeId> {
        Position::new(NodeId::new(change), ChangePosition::new(pos))
    }

    fn test_hash_position(pos: u64) -> Position<Hash> {
        Position::new(Hash::of(b"test"), ChangePosition::new(pos))
    }

    // Atom Tests

    #[test]
    fn test_atom_is_new_vertex() {
        let atom: Atom<Hash> = Atom::Insertion(Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        });

        assert!(atom.is_new_vertex());
        assert!(!atom.is_edge_map());
    }

    #[test]
    fn test_atom_is_edge_map() {
        let atom: Atom<Hash> = Atom::EdgeUpdate(EdgeUpdate {
            edges: vec![],
            inode: test_hash_position(0),
        });

        assert!(atom.is_edge_map());
        assert!(!atom.is_new_vertex());
    }

    #[test]
    fn test_atom_as_new_vertex() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };
        let atom: Atom<Hash> = Atom::Insertion(nv.clone());

        assert!(atom.as_new_vertex().is_some());
        assert!(atom.as_edge_map().is_none());
    }

    #[test]
    fn test_atom_inode() {
        let inode = test_hash_position(42);
        let atom: Atom<Hash> = Atom::Insertion(Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: inode.clone(),
        });

        assert_eq!(atom.inode().pos, inode.pos);
    }

    #[test]
    fn test_atom_from_new_vertex() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };
        let atom: Atom<Hash> = nv.into();
        assert!(atom.is_new_vertex());
    }

    #[test]
    fn test_atom_from_edge_map() {
        let em = EdgeUpdate {
            edges: vec![],
            inode: test_hash_position(0),
        };
        let atom: Atom<Hash> = em.into();
        assert!(atom.is_edge_map());
    }

    // Insertion Tests

    #[test]
    fn test_new_vertex_len() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(10),
            end: ChangePosition::new(50),
            inode: test_hash_position(0),
        };

        assert_eq!(nv.len(), 40);
        assert!(!nv.is_empty());
    }

    #[test]
    fn test_new_vertex_empty() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(10),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };

        assert!(nv.is_empty());
        assert_eq!(nv.len(), 0);
    }

    #[test]
    fn test_new_vertex_context() {
        let nv = Insertion {
            predecessors: vec![test_hash_position(0)],
            successors: vec![test_hash_position(10), test_hash_position(20)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };

        assert!(nv.has_predecessors());
        assert!(nv.has_successors());
        assert_eq!(nv.predecessors.len(), 1);
        assert_eq!(nv.successors.len(), 2);
    }

    #[test]
    fn test_new_vertex_no_context() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };

        assert!(!nv.has_predecessors());
        assert!(!nv.has_successors());
    }

    #[test]
    fn test_new_vertex_display() {
        let nv = Insertion {
            predecessors: vec![test_hash_position(0)],
            successors: vec![test_hash_position(10)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
            inode: test_hash_position(0),
        };

        let display = format!("{}", nv);
        assert!(display.contains("100"));
        assert!(display.contains("200"));
        assert!(display.contains("1 up"));
        assert!(display.contains("1 down"));
    }

    #[test]
    fn test_new_vertex_json_roundtrip() {
        let nv = Insertion {
            predecessors: vec![test_hash_position(0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        };

        let json = serde_json::to_string(&nv).unwrap();
        let parsed: Insertion<Hash> = serde_json::from_str(&json).unwrap();
        assert_eq!(nv, parsed);
    }

    #[test]
    fn test_new_vertex_postcard_roundtrip() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK | EdgeFlags::FOLDER,
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
            inode: test_hash_position(42),
        };

        let bytes = postcard::to_allocvec(&nv).unwrap();
        let parsed: Insertion<Hash> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(nv, parsed);
    }

    // EdgeUpdate Tests

    #[test]
    fn test_edge_map_new() {
        let inode = test_hash_position(42);
        let em = EdgeUpdate::<Hash>::new(inode.clone());

        assert!(em.is_empty());
        assert_eq!(em.len(), 0);
        assert_eq!(em.inode.pos, inode.pos);
    }

    #[test]
    fn test_edge_map_push() {
        let inode = test_hash_position(42);
        let mut em = EdgeUpdate::<Hash>::new(inode);

        em.push(NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"test"),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
            introduced_by: Hash::of(b"intro"),
        });

        assert!(!em.is_empty());
        assert_eq!(em.len(), 1);
    }

    #[test]
    fn test_edge_map_concat() {
        let inode = test_hash_position(42);
        let mut em1 = EdgeUpdate::<Hash>::new(inode.clone());
        let mut em2 = EdgeUpdate::<Hash>::new(inode);

        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"test"),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        em1.push(edge.clone());
        em2.push(edge);

        em1.concat(&em2);
        assert_eq!(em1.len(), 2);
    }

    #[test]
    fn test_edge_map_display() {
        let mut em = EdgeUpdate::<Hash>::new(test_hash_position(0));
        em.push(NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"test"),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
            introduced_by: Hash::of(b"intro"),
        });

        let display = format!("{}", em);
        assert!(display.contains("1 edges"));
    }

    #[test]
    fn test_edge_map_json_roundtrip() {
        let mut em = EdgeUpdate::<Hash>::new(test_hash_position(42));
        em.push(NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"intro"),
        });

        let json = serde_json::to_string(&em).unwrap();
        let parsed: EdgeUpdate<Hash> = serde_json::from_str(&json).unwrap();
        assert_eq!(em, parsed);
    }

    // NewEdge Tests

    #[test]
    fn test_new_edge_is_deletion() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        assert!(edge.is_deletion());
        assert!(!edge.is_undeletion());
        assert!(!edge.is_noop());
    }

    #[test]
    fn test_new_edge_is_undeletion() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            flag: EdgeFlags::BLOCK,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        assert!(edge.is_undeletion());
        assert!(!edge.is_deletion());
        assert!(!edge.is_noop());
    }

    #[test]
    fn test_new_edge_is_noop() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        assert!(edge.is_noop());
        assert!(!edge.is_deletion());
        assert!(!edge.is_undeletion());
    }

    #[test]
    fn test_new_edge_reverse() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"orig"),
        };

        let reverse_by = Hash::of(b"reverse");
        let reversed = edge.reverse(reverse_by);

        // Flags should be swapped
        assert_eq!(reversed.previous, edge.flag);
        assert_eq!(reversed.flag, edge.previous);
        assert_eq!(reversed.introduced_by, Hash::of(b"reverse"));

        // Positions should be preserved
        assert_eq!(reversed.from, edge.from);
        assert_eq!(reversed.to, edge.to);
    }

    #[test]
    fn test_new_edge_display() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"x"),
                ChangePosition::new(0),
                ChangePosition::new(5),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        let display = format!("{}", edge);
        assert!(display.contains("BLOCK"));
        assert!(display.contains("DELETED"));
    }

    #[test]
    fn test_new_edge_json_roundtrip() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: test_hash_position(100),
            to: GraphNode::new(
                Hash::of(b"dest"),
                ChangePosition::new(10),
                ChangePosition::new(20),
            ),
            introduced_by: Hash::of(b"introduced"),
        };

        let json = serde_json::to_string(&edge).unwrap();
        let parsed: NewEdge<Hash> = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, parsed);
    }

    #[test]
    fn test_new_edge_postcard_roundtrip() {
        let edge = NewEdge {
            previous: EdgeFlags::FOLDER | EdgeFlags::PARENT,
            flag: EdgeFlags::FOLDER | EdgeFlags::PARENT | EdgeFlags::DELETED,
            from: test_hash_position(0),
            to: GraphNode::new(
                Hash::of(b"dest"),
                ChangePosition::new(0),
                ChangePosition::new(100),
            ),
            introduced_by: Hash::of(b"intro"),
        };

        let bytes = postcard::to_allocvec(&edge).unwrap();
        let parsed: NewEdge<Hash> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(edge, parsed);
    }

    // Atom Serialization Tests

    #[test]
    fn test_atom_json_roundtrip() {
        let atom: Atom<Hash> = Atom::Insertion(Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        });

        let json = serde_json::to_string(&atom).unwrap();
        let parsed: Atom<Hash> = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_new_vertex());
        assert_eq!(atom, parsed);
    }

    #[test]
    fn test_atom_edge_map_json_roundtrip() {
        let atom: Atom<Hash> = Atom::EdgeUpdate(EdgeUpdate {
            edges: vec![],
            inode: test_hash_position(0),
        });

        let json = serde_json::to_string(&atom).unwrap();
        let parsed: Atom<Hash> = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_edge_map());
        assert_eq!(atom, parsed);
    }

    #[test]
    fn test_atom_postcard_roundtrip() {
        let atoms: Vec<Atom<Hash>> = vec![
            Atom::Insertion(Insertion {
                predecessors: vec![test_hash_position(0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(100),
                inode: test_hash_position(42),
            }),
            Atom::EdgeUpdate(EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::DELETED,
                    from: test_hash_position(0),
                    to: GraphNode::new(
                        Hash::of(b"x"),
                        ChangePosition::new(0),
                        ChangePosition::new(5),
                    ),
                    introduced_by: Hash::of(b"intro"),
                }],
                inode: test_hash_position(42),
            }),
        ];

        for atom in atoms {
            let bytes = postcard::to_allocvec(&atom).unwrap();
            let parsed: Atom<Hash> = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(atom, parsed);
        }
    }

    // Edge Cases

    #[test]
    fn test_new_vertex_max_values() {
        let nv = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::all(),
            start: ChangePosition::new(u64::MAX - 1),
            end: ChangePosition::new(u64::MAX),
            inode: test_hash_position(u64::MAX),
        };

        assert_eq!(nv.len(), 1);
        assert!(!nv.is_empty());
    }

    #[test]
    fn test_edge_map_many_edges() {
        let mut em = EdgeUpdate::<Hash>::new(test_hash_position(0));

        for i in 0..100 {
            em.push(NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::DELETED,
                from: test_hash_position(i),
                to: GraphNode::new(
                    Hash::of(&[i as u8]),
                    ChangePosition::new(0),
                    ChangePosition::new(1),
                ),
                introduced_by: Hash::of(b"intro"),
            });
        }

        assert_eq!(em.len(), 100);

        // Verify serialization still works
        let bytes = postcard::to_allocvec(&em).unwrap();
        let parsed: EdgeUpdate<Hash> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(em, parsed);
    }

    #[test]
    fn test_atom_to_option() {
        let atom: Atom<Hash> = Atom::Insertion(Insertion {
            predecessors: vec![test_hash_position(0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(42),
        });

        let opt_atom = atom.to_option();
        assert!(opt_atom.is_new_vertex());

        if let Atom::Insertion(nv) = opt_atom {
            assert!(nv.predecessors[0].change.is_some());
            assert!(nv.inode.change.is_some());
        }
    }
}

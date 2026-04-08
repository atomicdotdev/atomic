//! Compact graph types for V3 serialization.
//!
//! Space-efficient versions of the core graph types that use [`HashIndex`]
//! references (2 bytes) instead of full 32-byte hashes. These types exist
//! **solely for serialization** — they are never used in the in-memory graph.

use super::super::types::{CompactPosition, HashIndex, HASH_INDEX_NONE, HASH_INDEX_SELF};
use serde::{Deserialize, Serialize};
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// CompactGraphNode — GraphNode<Option<Hash>> → (HashIndex, u32, u32)
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`GraphNode<Option<Hash>>`](crate::GraphNode).
///
/// Replaces the 33-byte `Option<Hash>` with a 1-3 byte `HashIndex` varint
/// and the two 8-byte `u64` positions with 1-5 byte `u32` varints.
///
/// # Size Comparison
///
/// | Encoding | `change` | `start` | `end` | Total |
/// |----------|----------|---------|-------|-------|
/// | V2 (bincode) | 33 bytes | 8 bytes | 8 bytes | 49 bytes |
/// | V3 (postcard, typical) | 1 byte | 1-2 bytes | 1-2 bytes | 3-5 bytes |
/// | V3 (postcard, max) | 3 bytes | 5 bytes | 5 bytes | 13 bytes |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CompactGraphNode {
    /// Index into the hash dedup table identifying the change.
    ///
    /// - `0` = this change itself
    /// - `0xFFFF` = root / none
    /// - Other = dependency change
    pub change: HashIndex,

    /// Start position within the change's content (inclusive).
    pub start: u32,

    /// End position within the change's content (exclusive).
    pub end: u32,
}

impl CompactGraphNode {
    /// Create a new compact graph node.
    #[inline]
    pub const fn new(change: HashIndex, start: u32, end: u32) -> Self {
        Self { change, start, end }
    }

    /// Create a self-referencing graph node (references this change's content).
    #[inline]
    pub const fn self_ref(start: u32, end: u32) -> Self {
        Self {
            change: HASH_INDEX_SELF,
            start,
            end,
        }
    }

    /// Create a root graph node.
    #[inline]
    pub const fn root(start: u32, end: u32) -> Self {
        Self {
            change: HASH_INDEX_NONE,
            start,
            end,
        }
    }

    /// Returns `true` if this is a root node (no associated change).
    #[inline]
    pub const fn is_root(&self) -> bool {
        self.change == HASH_INDEX_NONE
    }

    /// Returns `true` if this node references the change's own content.
    #[inline]
    pub const fn is_self_ref(&self) -> bool {
        self.change == HASH_INDEX_SELF
    }

    /// Returns the content length in bytes.
    #[inline]
    pub const fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Returns `true` if this is an empty node (start == end).
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl fmt::Display for CompactGraphNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            write!(f, "ROOT[{}:{}]", self.start, self.end)
        } else if self.is_self_ref() {
            write!(f, "SELF[{}:{}]", self.start, self.end)
        } else {
            write!(f, "#{}[{}:{}]", self.change, self.start, self.end)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactInsertion — Insertion<Option<Hash>> using compact positions
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`Insertion<Option<Hash>>`](crate::change::atom::Insertion).
///
/// All position fields use [`CompactPosition`] instead of `Position<Option<Hash>>`,
/// and byte offsets use `u32` instead of `u64`.
///
/// # Size Comparison (typical: 2 predecessors, 0 successors)
///
/// | Field | V2 (bincode) | V3 (postcard) |
/// |-------|-------------|---------------|
/// | `predecessors` (2×) | 8 + 2×41 = 90 bytes | 1 + 2×2 = 5 bytes |
/// | `successors` (0×) | 8 bytes | 1 byte |
/// | `flag` | 1 byte | 1 byte |
/// | `start` | 8 bytes | 1-2 bytes |
/// | `end` | 8 bytes | 1-2 bytes |
/// | `inode` | 41 bytes | 2-3 bytes |
/// | **Total** | **156 bytes** | **11-14 bytes** |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactInsertion {
    /// Vertices that should come before this new content.
    pub predecessors: Vec<CompactPosition>,

    /// Vertices that should come after this new content.
    pub successors: Vec<CompactPosition>,

    /// Edge flags (BLOCK, FOLDER, etc.).
    pub flag: u8,

    /// Start offset in the change's content blob (inclusive).
    pub start: u32,

    /// End offset in the change's content blob (exclusive).
    pub end: u32,

    /// The file (inode) this vertex belongs to.
    pub inode: CompactPosition,
}

impl CompactInsertion {
    /// Returns the content length in bytes.
    #[inline]
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Returns `true` if this is an empty vertex (start == end).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Returns `true` if this vertex has predecessors.
    #[inline]
    pub fn has_predecessors(&self) -> bool {
        !self.predecessors.is_empty()
    }

    /// Returns `true` if this vertex has successors.
    #[inline]
    pub fn has_successors(&self) -> bool {
        !self.successors.is_empty()
    }
}

impl fmt::Display for CompactInsertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CompactInsertion[{}..{}] ({} up, {} down)",
            self.start,
            self.end,
            self.predecessors.len(),
            self.successors.len()
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactNewEdge — NewEdge<Option<Hash>> using compact positions
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`NewEdge<Option<Hash>>`](crate::change::atom::NewEdge).
///
/// Replaces full hash positions with compact index-based positions.
///
/// # Size Comparison
///
/// | Field | V2 (bincode) | V3 (postcard) |
/// |-------|-------------|---------------|
/// | `previous` | 1 byte | 1 byte |
/// | `flag` | 1 byte | 1 byte |
/// | `from` (Position) | 41 bytes | 2-5 bytes |
/// | `to` (GraphNode) | 49 bytes | 3-8 bytes |
/// | `introduced_by` | 33 bytes | 1-3 bytes |
/// | **Total** | **125 bytes** | **8-18 bytes** |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactNewEdge {
    /// The flags the edge currently has.
    pub previous: u8,

    /// The flags the edge should have after modification.
    pub flag: u8,

    /// Source position of the edge.
    pub from: CompactPosition,

    /// Destination node of the edge.
    pub to: CompactGraphNode,

    /// Change that originally introduced this edge.
    ///
    /// - `HASH_INDEX_SELF` (0) = this change
    /// - `HASH_INDEX_NONE` (0xFFFF) = root
    /// - Other = dependency change index
    pub introduced_by: HashIndex,
}

impl fmt::Display for CompactNewEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Edge(0x{:02X}→0x{:02X}, {} → {}, by #{})",
            self.previous, self.flag, self.from, self.to, self.introduced_by
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactEdgeUpdate — EdgeUpdate<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`EdgeUpdate<Option<Hash>>`](crate::change::atom::EdgeUpdate).
///
/// Contains a list of compact edge modifications and the file's inode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactEdgeUpdate {
    /// The edge modifications to apply.
    pub edges: Vec<CompactNewEdge>,

    /// The file (inode) these edges belong to.
    pub inode: CompactPosition,
}

impl CompactEdgeUpdate {
    /// Returns `true` if there are no edge modifications.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Returns the number of edge modifications.
    #[inline]
    pub fn len(&self) -> usize {
        self.edges.len()
    }
}

impl fmt::Display for CompactEdgeUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CompactEdgeUpdate({} edges)", self.edges.len())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactAtom — Atom<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`Atom<Option<Hash>>`](crate::change::atom::Atom).
///
/// An atomic graph operation: either an insertion of new content or a
/// modification of existing edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactAtom {
    /// Insert new content into the graph.
    Insertion(CompactInsertion),
    /// Modify existing edges.
    EdgeUpdate(CompactEdgeUpdate),
}

impl CompactAtom {
    /// Returns `true` if this is an insertion.
    #[inline]
    pub fn is_insertion(&self) -> bool {
        matches!(self, CompactAtom::Insertion(_))
    }

    /// Returns `true` if this is an edge update.
    #[inline]
    pub fn is_edge_update(&self) -> bool {
        matches!(self, CompactAtom::EdgeUpdate(_))
    }
}

impl fmt::Display for CompactAtom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompactAtom::Insertion(v) => write!(f, "{}", v),
            CompactAtom::EdgeUpdate(e) => write!(f, "{}", e),
        }
    }
}

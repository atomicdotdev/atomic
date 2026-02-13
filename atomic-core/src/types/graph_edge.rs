//! GraphEdge types and flags for the Atomic graph
//!
//! GraphEdges connect GraphNodes in the repository graph. Each edge has:
//! - Flags indicating the edge type (block, folder, deleted, etc.)
//! - A destination position
//! - A reference to the change that introduced the edge

use super::{ChangePosition, NodeId, Position, L64};
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::fmt;

bitflags! {
    /// Flags that describe the type and state of an edge.
    ///
    /// Edges can have multiple flags set simultaneously. For example,
    /// a deleted folder edge would have both `FOLDER` and `DELETED` set.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
    pub struct EdgeFlags: u8 {
        /// A block edge connects sequential content within a file.
        /// This is the most common edge type for file contents.
        const BLOCK = 0b0000_0001;

        /// A pseudo-edge is computed during change application to maintain
        /// graph connectivity. These are not explicitly stored in changes
        /// but are derived from the graph structure.
        const PSEUDO = 0b0000_0100;

        /// A folder edge represents file system hierarchy.
        /// These connect directory vertices to their children.
        const FOLDER = 0b0001_0000;

        /// A parent edge is the reverse direction of another edge.
        /// For every forward edge, there is a corresponding parent edge
        /// enabling efficient bidirectional traversal.
        const PARENT = 0b0010_0000;

        /// A deleted edge marks content that has been removed.
        /// The content remains in the graph but is no longer "alive".
        const DELETED = 0b1000_0000;
    }
}

impl EdgeFlags {
    /// Check if the edge represents alive (non-deleted) content
    #[inline]
    pub fn is_alive(self) -> bool {
        !self.contains(Self::DELETED)
    }

    /// Check if this is a parent (reverse) edge
    #[inline]
    pub fn is_parent(self) -> bool {
        self.contains(Self::PARENT)
    }

    /// Check if this is a folder edge
    #[inline]
    pub fn is_folder(self) -> bool {
        self.contains(Self::FOLDER)
    }

    /// Check if this is a block edge
    #[inline]
    pub fn is_block(self) -> bool {
        self.contains(Self::BLOCK)
    }

    /// Check if this is a pseudo-edge
    #[inline]
    pub fn is_pseudo(self) -> bool {
        self.contains(Self::PSEUDO)
    }

    /// Check if this is a deleted edge
    #[inline]
    pub fn is_deleted(self) -> bool {
        self.contains(Self::DELETED)
    }

    /// Create flags for an alive parent edge
    #[inline]
    pub fn alive_parent() -> Self {
        Self::PARENT
    }

    /// Create flags for a deleted folder edge
    #[inline]
    pub fn deleted_folder() -> Self {
        Self::DELETED | Self::FOLDER
    }

    /// Create flags for a block + parent edge
    #[inline]
    pub fn block_parent() -> Self {
        Self::BLOCK | Self::PARENT
    }

    /// Create flags for a pseudo folder edge
    #[inline]
    pub fn pseudo_folder() -> Self {
        Self::PSEUDO | Self::FOLDER
    }

    /// Create flags for alive children traversal
    #[inline]
    pub fn alive_children() -> Self {
        Self::BLOCK | Self::PSEUDO | Self::FOLDER
    }

    /// Create flags for a parent folder edge
    #[inline]
    pub fn parent_folder() -> Self {
        Self::PARENT | Self::FOLDER
    }

    /// Check if this edge represents an alive parent
    #[inline]
    pub fn is_alive_parent(self) -> bool {
        (self & (Self::DELETED | Self::PARENT)) == Self::PARENT
    }
}

impl Default for EdgeFlags {
    fn default() -> Self {
        Self::BLOCK
    }
}

impl fmt::Display for EdgeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.contains(Self::BLOCK) {
            parts.push("BLOCK");
        }
        if self.contains(Self::PSEUDO) {
            parts.push("PSEUDO");
        }
        if self.contains(Self::FOLDER) {
            parts.push("FOLDER");
        }
        if self.contains(Self::PARENT) {
            parts.push("PARENT");
        }
        if self.contains(Self::DELETED) {
            parts.push("DELETED");
        }
        if parts.is_empty() {
            write!(f, "NONE")
        } else {
            write!(f, "{}", parts.join("|"))
        }
    }
}

/// Full edge representation for API use.
///
/// This is the "expanded" form of an edge, convenient for working with
/// but larger than the serialized form used in storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphEdge {
    /// Flags describing the edge type
    pub flag: EdgeFlags,
    /// Destination of the edge
    pub dest: Position<NodeId>,
    /// The change that introduced this edge
    pub introduced_by: NodeId,
}

impl GraphEdge {
    /// Create a new edge
    #[inline]
    pub fn new(flag: EdgeFlags, dest: Position<NodeId>, introduced_by: NodeId) -> Self {
        Self {
            flag,
            dest,
            introduced_by,
        }
    }

    /// Create a reverse (parent) edge from this edge
    pub fn reverse(&self, source_end: Position<NodeId>) -> Self {
        Self {
            flag: self.flag | EdgeFlags::PARENT,
            dest: source_end,
            introduced_by: self.introduced_by,
        }
    }
}

/// Compact serialized edge for storage.
///
/// Layout: `[flags:8][pos:56] [change:64] [introduced_by:64]`
///
/// This packs the edge flags into the high byte of the first u64,
/// with the position in the low 56 bits. This limits positions to
/// 2^56 bytes (~72 petabytes), which should be sufficient.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SerializedGraphEdge([L64; 3]);

impl SerializedGraphEdge {
    /// Maximum position value (56 bits)
    const MAX_POSITION: u64 = (1 << 56) - 1;

    /// Create a new serialized edge
    ///
    /// # Panics
    ///
    /// Panics if the position exceeds 56 bits.
    pub fn new(flag: EdgeFlags, dest: Position<NodeId>, introduced_by: NodeId) -> Self {
        let pos_bits = dest.pos.get();
        assert!(
            pos_bits <= Self::MAX_POSITION,
            "Position {} exceeds maximum {}",
            pos_bits,
            Self::MAX_POSITION
        );

        let flag_bits = (flag.bits() as u64) << 56;
        Self([
            L64::new(flag_bits | pos_bits),
            dest.change.0,
            introduced_by.0,
        ])
    }

    /// Create an edge with empty flags (used for cursor positioning)
    pub fn empty(dest: Position<NodeId>, introduced_by: NodeId) -> Self {
        Self([dest.pos.0, dest.change.0, introduced_by.0])
    }

    /// Get the edge flags
    #[inline]
    pub fn flag(&self) -> EdgeFlags {
        let raw = (self.0[0].get() >> 56) as u8;
        EdgeFlags::from_bits_truncate(raw)
    }

    /// Get the destination position
    #[inline]
    pub fn dest(&self) -> Position<NodeId> {
        Position {
            change: NodeId(self.0[1]),
            pos: ChangePosition(L64::new(self.0[0].get() & Self::MAX_POSITION)),
        }
    }

    /// Get the change that introduced this edge
    #[inline]
    pub fn introduced_by(&self) -> NodeId {
        NodeId(self.0[2])
    }

    /// Convert to the expanded GraphEdge representation
    #[inline]
    pub fn to_edge(&self) -> GraphEdge {
        GraphEdge {
            flag: self.flag(),
            dest: self.dest(),
            introduced_by: self.introduced_by(),
        }
    }
}

impl fmt::Debug for SerializedGraphEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dest = self.dest();
        write!(
            f,
            "GraphEdge({}, {}@{}, by={})",
            self.flag(),
            dest.change,
            dest.pos,
            self.introduced_by()
        )
    }
}

impl From<GraphEdge> for SerializedGraphEdge {
    fn from(edge: GraphEdge) -> Self {
        SerializedGraphEdge::new(edge.flag, edge.dest, edge.introduced_by)
    }
}

impl From<SerializedGraphEdge> for GraphEdge {
    fn from(serialized: SerializedGraphEdge) -> Self {
        serialized.to_edge()
    }
}

impl From<&SerializedGraphEdge> for GraphEdge {
    fn from(serialized: &SerializedGraphEdge) -> Self {
        serialized.to_edge()
    }
}

/// Remove flags from a serialized edge (used for edge updates)
impl std::ops::SubAssign<EdgeFlags> for SerializedGraphEdge {
    fn sub_assign(&mut self, flags: EdgeFlags) {
        let current = self.0[0].get();
        let current_flags = (current >> 56) as u8;
        let new_flags = current_flags & !flags.bits();
        let new_value = ((new_flags as u64) << 56) | (current & Self::MAX_POSITION);
        self.0[0] = L64::new(new_value);
    }
}

/// Add flags to a serialized edge (used for edge updates)
impl std::ops::AddAssign<EdgeFlags> for SerializedGraphEdge {
    fn add_assign(&mut self, flags: EdgeFlags) {
        let current = self.0[0].get();
        let current_flags = (current >> 56) as u8;
        let new_flags = current_flags | flags.bits();
        let new_value = ((new_flags as u64) << 56) | (current & Self::MAX_POSITION);
        self.0[0] = L64::new(new_value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_flags_combinations() {
        let deleted_folder = EdgeFlags::deleted_folder();
        assert!(deleted_folder.is_deleted());
        assert!(deleted_folder.is_folder());
        assert!(!deleted_folder.is_block());

        let block_parent = EdgeFlags::block_parent();
        assert!(block_parent.is_block());
        assert!(block_parent.is_parent());
        assert!(!block_parent.is_deleted());
    }

    #[test]
    fn test_edge_flags_alive_check() {
        assert!(EdgeFlags::BLOCK.is_alive());
        assert!(EdgeFlags::FOLDER.is_alive());
        assert!(!EdgeFlags::DELETED.is_alive());
        assert!(!(EdgeFlags::DELETED | EdgeFlags::BLOCK).is_alive());
    }

    #[test]
    fn test_edge_flags_display() {
        assert_eq!(EdgeFlags::BLOCK.to_string(), "BLOCK");
        assert_eq!(
            (EdgeFlags::BLOCK | EdgeFlags::PARENT).to_string(),
            "BLOCK|PARENT"
        );
        assert_eq!(EdgeFlags::empty().to_string(), "NONE");
    }

    #[test]
    fn test_serialized_edge_roundtrip() {
        let edge = GraphEdge {
            flag: EdgeFlags::BLOCK | EdgeFlags::PARENT,
            dest: Position {
                change: NodeId::new(42),
                pos: ChangePosition::new(1000),
            },
            introduced_by: NodeId::new(7),
        };

        let serialized = SerializedGraphEdge::from(edge);
        let recovered = GraphEdge::from(serialized);

        assert_eq!(edge.flag, recovered.flag);
        assert_eq!(edge.dest.change, recovered.dest.change);
        assert_eq!(edge.dest.pos, recovered.dest.pos);
        assert_eq!(edge.introduced_by, recovered.introduced_by);
    }

    #[test]
    fn test_serialized_edge_flag_modification() {
        let dest = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(100),
        };
        let mut edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(5));

        assert_eq!(edge.flag(), EdgeFlags::BLOCK);

        edge += EdgeFlags::DELETED;
        assert_eq!(edge.flag(), EdgeFlags::BLOCK | EdgeFlags::DELETED);

        edge -= EdgeFlags::BLOCK;
        assert_eq!(edge.flag(), EdgeFlags::DELETED);
    }

    #[test]
    fn test_serialized_edge_ordering() {
        let dest1 = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(100),
        };
        let dest2 = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(200),
        };

        let edge1 = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest1, NodeId::new(1));
        let edge2 = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest2, NodeId::new(1));

        // Edges with lower positions should sort first
        assert!(edge1 < edge2);
    }

    #[test]
    fn test_max_position() {
        let max_pos = (1u64 << 56) - 1;
        let dest = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(max_pos),
        };
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));
        assert_eq!(edge.dest().pos.get(), max_pos);
    }

    #[test]
    #[should_panic(expected = "exceeds maximum")]
    fn test_position_overflow() {
        let too_large = 1u64 << 56;
        let dest = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(too_large),
        };
        let _ = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1));
    }
}

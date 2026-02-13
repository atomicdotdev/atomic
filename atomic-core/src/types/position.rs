//! Position type for addressing content within changes
//!
//! A Position uniquely identifies a byte location within a change's content,
//! combining the change identifier with a byte offset.

use super::node_id::{ChangePosition, NodeId};
use super::Base32;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A specific byte position within a change.
///
/// Position combines a change reference (NodeId or Hash) with a byte offset
/// (ChangePosition) to uniquely identify any location in the repository's
/// content history.
///
/// # Type Parameter
///
/// - `H`: The type of change identifier. Typically `NodeId` for internal use
///   or `Hash` for external/serialized representation.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position<H> {
    /// The change that contains this position
    pub change: H,
    /// The byte offset within the change's content
    pub pos: ChangePosition,
}

impl<H> Position<H> {
    /// Create a new Position
    #[inline]
    pub const fn new(change: H, pos: ChangePosition) -> Self {
        Position { change, pos }
    }
}

impl Position<NodeId> {
    /// The root position (change 0, position 0)
    pub const ROOT: Position<NodeId> = Position {
        change: NodeId::ROOT,
        pos: ChangePosition::ROOT,
    };

    /// The bottom sentinel position
    pub const BOTTOM: Position<NodeId> = Position {
        change: NodeId::ROOT,
        pos: ChangePosition::BOTTOM,
    };

    /// Check if this is the root position
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Convert to an Option-wrapped position
    #[inline]
    pub fn to_option(&self) -> Position<Option<NodeId>> {
        Position {
            change: Some(self.change),
            pos: self.pos,
        }
    }

    /// Create an inode node from this position.
    ///
    /// An inode node is a zero-length node at this position,
    /// used as the root of a file's content graph.
    ///
    /// # Returns
    ///
    /// A GraphNode with `start == end == self.pos`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::types::{Position, NodeId, ChangePosition};
    ///
    /// let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
    /// let inode_node = pos.inode_node();
    ///
    /// assert_eq!(inode_node.change, NodeId::new(42));
    /// assert_eq!(inode_node.start, ChangePosition::new(100));
    /// assert_eq!(inode_node.end, ChangePosition::new(100));
    /// assert!(inode_node.is_empty());
    /// ```
    #[inline]
    pub fn inode_node(&self) -> super::GraphNode<NodeId> {
        super::GraphNode {
            change: self.change,
            start: self.pos,
            end: self.pos,
        }
    }
}

impl<H> Position<Option<H>> {
    /// Unwrap an optional position
    ///
    /// # Panics
    ///
    /// Panics if the change is None
    #[inline]
    pub fn unwrap(self) -> Position<H> {
        Position {
            change: self.change.unwrap(),
            pos: self.pos,
        }
    }

    /// Try to unwrap an optional position
    #[inline]
    pub fn try_unwrap(self) -> Option<Position<H>> {
        Some(Position {
            change: self.change?,
            pos: self.pos,
        })
    }
}

impl<H: Clone> Position<H> {
    /// Add an offset to the position
    #[inline]
    pub fn offset(&self, offset: usize) -> Self {
        Position {
            change: self.change.clone(),
            pos: self.pos + offset,
        }
    }
}

impl<H> std::ops::Add<usize> for Position<H> {
    type Output = Position<H>;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        Position {
            change: self.change,
            pos: self.pos + rhs,
        }
    }
}

impl<H: fmt::Debug> fmt::Debug for Position<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pos({:?}[{}])", self.change, self.pos.get())
    }
}

impl<H: fmt::Display> fmt::Display for Position<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.change, self.pos.get())
    }
}

/// Base32 alphabet for encoding (RFC 4648, no padding)
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl<H: Base32> Base32 for Position<H> {
    fn to_base32(&self) -> String {
        let mut result = self.change.to_base32();
        result.push('.');

        // Encode position as variable-length base32
        let pos_value = self.pos.get();
        if pos_value == 0 {
            result.push('A');
        } else {
            // Find the number of 5-bit groups needed
            let bits_needed = 64 - pos_value.leading_zeros();
            let groups = (bits_needed as usize + 4) / 5;

            for i in (0..groups).rev() {
                let idx = ((pos_value >> (i * 5)) & 0x1F) as usize;
                result.push(BASE32_ALPHABET[idx] as char);
            }
        }

        result
    }

    fn from_base32(s: &[u8]) -> Option<Self> {
        // Find the separator
        let dot_pos = s.iter().position(|&c| c == b'.')?;
        let (change_part, pos_part) = s.split_at(dot_pos);
        let pos_part = &pos_part[1..]; // Skip the dot

        let change = H::from_base32(change_part)?;

        // Decode position
        let mut pos_value: u64 = 0;
        for &c in pos_part {
            let c = c.to_ascii_uppercase();
            let idx = match c {
                b'A'..=b'Z' => c - b'A',
                b'2'..=b'7' => c - b'2' + 26,
                _ => return None,
            };
            pos_value = pos_value.checked_mul(32)?.checked_add(idx as u64)?;
        }

        Some(Position {
            change,
            pos: ChangePosition::new(pos_value),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_creation() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        assert_eq!(pos.change.get(), 42);
        assert_eq!(pos.pos.get(), 100);
    }

    #[test]
    fn test_position_root() {
        assert!(Position::<NodeId>::ROOT.is_root());
        assert!(!Position::new(NodeId::new(1), ChangePosition::ROOT).is_root());
    }

    #[test]
    fn test_position_offset() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(10));
        let offset_pos = pos.offset(5);
        assert_eq!(offset_pos.change.get(), 1);
        assert_eq!(offset_pos.pos.get(), 15);
    }

    #[test]
    fn test_position_add() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(10));
        let new_pos = pos + 20;
        assert_eq!(new_pos.pos.get(), 30);
    }

    #[test]
    fn test_position_option_unwrap() {
        let opt_pos: Position<Option<NodeId>> = Position {
            change: Some(NodeId::new(5)),
            pos: ChangePosition::new(10),
        };
        let pos = opt_pos.unwrap();
        assert_eq!(pos.change.get(), 5);
    }

    #[test]
    fn test_position_to_option() {
        let pos = Position::new(NodeId::new(5), ChangePosition::new(10));
        let opt = pos.to_option();
        assert_eq!(opt.change, Some(NodeId::new(5)));
    }

    #[test]
    fn test_position_serialization() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let json = serde_json::to_string(&pos).unwrap();
        let parsed: Position<NodeId> = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, parsed);
    }

    #[test]
    fn test_position_inode_node() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let inode_node = pos.inode_node();

        assert_eq!(inode_node.change, NodeId::new(42));
        assert_eq!(inode_node.start, ChangePosition::new(100));
        assert_eq!(inode_node.end, ChangePosition::new(100));
        assert!(inode_node.is_empty());
    }

    #[test]
    fn test_position_inode_node_root() {
        let pos = Position::ROOT;
        let inode_node = pos.inode_node();

        assert_eq!(inode_node.change, NodeId::ROOT);
        assert!(inode_node.is_empty());
    }

    #[test]
    fn test_position_ordering() {
        let pos1 = Position::new(NodeId::new(1), ChangePosition::new(10));
        let pos2 = Position::new(NodeId::new(1), ChangePosition::new(20));
        let pos3 = Position::new(NodeId::new(2), ChangePosition::new(5));

        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
        assert!(pos1 < pos3);
    }
}

//! Node identifiers and position types
//!
//! This module defines the low-level identifier types used to reference
//! nodes, changes, and positions within the Atomic graph.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A 64-bit little-endian integer wrapper for cross-platform consistency.
///
/// This ensures that all numeric values are stored in a consistent byte order
/// regardless of the host platform's native endianness.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct L64(pub u64);

impl L64 {
    /// Create a new L64 from a native u64 value
    #[inline]
    pub const fn new(value: u64) -> Self {
        L64(value.to_le())
    }

    /// Get the native u64 value
    #[inline]
    pub const fn get(self) -> u64 {
        u64::from_le(self.0)
    }

    /// Convert to little-endian bytes
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Create from little-endian bytes
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 8]) -> Self {
        L64(u64::from_le_bytes(bytes))
    }

    /// Write to a byte slice (little-endian)
    #[inline]
    pub fn to_slice_le(self, buf: &mut [u8; 8]) {
        *buf = self.to_le_bytes();
    }

    /// Read from a byte slice (little-endian)
    #[inline]
    pub fn from_slice_le(buf: &[u8; 8]) -> Self {
        Self::from_le_bytes(*buf)
    }

    /// Get the underlying value as u64 (already in LE storage)
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.get()
    }
}

impl From<u64> for L64 {
    #[inline]
    fn from(value: u64) -> Self {
        L64::new(value)
    }
}

impl From<L64> for u64 {
    #[inline]
    fn from(value: L64) -> Self {
        value.get()
    }
}

impl std::ops::Add<usize> for L64 {
    type Output = L64;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        L64::new(self.get() + rhs as u64)
    }
}

impl std::ops::Sub<L64> for L64 {
    type Output = u64;

    #[inline]
    fn sub(self, rhs: L64) -> Self::Output {
        self.get() - rhs.get()
    }
}

impl fmt::Debug for L64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L64({})", self.get())
    }
}

impl fmt::Display for L64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl Serialize for L64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for L64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(L64::new)
    }
}

/// Internal node identifier (repository-local).
///
/// NodeIds are assigned sequentially within a repository and are used as
/// compact references to changes and other nodes in the graph. They are
/// not meaningful outside of the repository that created them.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub L64);

impl NodeId {
    /// The root node identifier (always 0)
    pub const ROOT: NodeId = NodeId(L64(0));

    /// The maximum possible node ID
    pub const MAX: NodeId = NodeId(L64(u64::MAX));

    /// Create a new NodeId from a u64 value
    #[inline]
    pub const fn new(value: u64) -> Self {
        NodeId(L64::new(value))
    }

    /// Get the underlying u64 value
    #[inline]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// Check if this is the root node
    #[inline]
    pub fn is_root(self) -> bool {
        self == Self::ROOT
    }

    /// Get the next sequential NodeId
    #[inline]
    pub fn next(self) -> Self {
        NodeId::new(self.get() + 1)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.get())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<u64> for NodeId {
    #[inline]
    fn from(value: u64) -> Self {
        NodeId::new(value)
    }
}

impl From<NodeId> for u64 {
    #[inline]
    fn from(value: NodeId) -> Self {
        value.get()
    }
}

/// Position within a change's content buffer.
///
/// ChangePosition identifies a specific byte offset within a change's
/// content blob. Combined with a NodeId, it forms a complete Position.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChangePosition(pub L64);

impl ChangePosition {
    /// The root position (0)
    pub const ROOT: ChangePosition = ChangePosition(L64(0));

    /// The "bottom" sentinel position (1)
    /// Used for special graph nodes
    pub const BOTTOM: ChangePosition = ChangePosition(L64(1u64.to_le()));

    /// Create a new ChangePosition from a u64 value
    #[inline]
    pub const fn new(value: u64) -> Self {
        ChangePosition(L64::new(value))
    }

    /// Get the underlying u64 value
    #[inline]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// Convert to usize (for indexing)
    #[inline]
    pub fn as_usize(self) -> usize {
        self.get() as usize
    }
}

impl fmt::Debug for ChangePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangePosition({})", self.get())
    }
}

impl fmt::Display for ChangePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<u64> for ChangePosition {
    #[inline]
    fn from(value: u64) -> Self {
        ChangePosition::new(value)
    }
}

impl From<usize> for ChangePosition {
    #[inline]
    fn from(value: usize) -> Self {
        ChangePosition::new(value as u64)
    }
}

impl std::ops::Add<usize> for ChangePosition {
    type Output = ChangePosition;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        ChangePosition::new(self.get() + rhs as u64)
    }
}

impl std::ops::Sub<ChangePosition> for ChangePosition {
    type Output = usize;

    #[inline]
    fn sub(self, rhs: ChangePosition) -> Self::Output {
        (self.get() - rhs.get()) as usize
    }
}

/// File system inode identifier.
///
/// Inodes provide a stable reference to files and directories that persists
/// across renames. This allows Atomic to track file identity independent
/// of path.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Inode(pub L64);

impl Inode {
    /// The root directory inode
    pub const ROOT: Inode = Inode(L64(0));

    /// Create a new Inode from a u64 value
    #[inline]
    pub const fn new(value: u64) -> Self {
        Inode(L64::new(value))
    }

    /// Get the underlying u64 value
    #[inline]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// Check if this is the root inode
    #[inline]
    pub fn is_root(self) -> bool {
        self == Self::ROOT
    }

    /// Get the next sequential Inode
    #[inline]
    pub fn next(self) -> Self {
        Inode::new(self.get() + 1)
    }
}

impl fmt::Debug for Inode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Inode({})", self.get())
    }
}

impl fmt::Display for Inode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<u64> for Inode {
    #[inline]
    fn from(value: u64) -> Self {
        Inode::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l64_endianness() {
        let value = 0x0102030405060708u64;
        let l64 = L64::new(value);
        assert_eq!(l64.get(), value);

        // Verify byte order
        let bytes = l64.to_le_bytes();
        assert_eq!(bytes, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn test_l64_arithmetic() {
        let a = L64::new(100);
        let b = L64::new(30);
        assert_eq!(a - b, 70);
        assert_eq!((a + 5usize).get(), 105);
    }

    #[test]
    fn test_node_id() {
        assert!(NodeId::ROOT.is_root());
        assert!(!NodeId::new(1).is_root());
        assert_eq!(NodeId::ROOT.next(), NodeId::new(1));
    }

    #[test]
    fn test_change_position_arithmetic() {
        let pos = ChangePosition::new(100);
        let pos2 = pos + 50;
        assert_eq!(pos2.get(), 150);
        assert_eq!(pos2 - pos, 50);
    }

    #[test]
    fn test_inode() {
        assert!(Inode::ROOT.is_root());
        assert_eq!(Inode::ROOT.next(), Inode::new(1));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let node_id = NodeId::new(12345);
        let json = serde_json::to_string(&node_id).unwrap();
        let parsed: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(node_id, parsed);
    }
}

//! Unique identifiers for the hierarchical CRDT graph model.
//!
//! This module defines the ID types used to uniquely identify elements
//! in the Trunk → Branch → Leaf hierarchy. Each ID is globally unique
//! and immutable, enabling CRDT conflict-free operations.
//!
//! # ID Types
//!
//! | Type | Level | Description |
//! |------|-------|-------------|
//! | [`TrunkId`] | File | Identifies a file in the repository |
//! | [`BranchId`] | Line | Identifies a line within a file |
//! | [`LeafId`] | Token | Identifies a token within a line |
//!
//! # Design Principles
//!
//! 1. **Global Uniqueness**: Each ID embeds its creating change, ensuring
//!    uniqueness without coordination between replicas.
//!
//! 2. **Immutability**: IDs never change, even for deleted content.
//!    This enables safe concurrent references.
//!
//! 3. **Deterministic Ordering**: IDs implement [`Ord`] for CRDT conflict
//!    resolution - concurrent inserts are ordered by ID.
//!
//! 4. **Compact Storage**: All IDs are 12 bytes (8-byte NodeId + 4-byte index).

use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::fmt;

// TrunkId - File Level Identifier

/// Unique identifier for a file (trunk) in the CRDT graph.
///
/// Combines the creating change with an index, ensuring global uniqueness.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::TrunkId;
/// use atomic_core::types::NodeId;
///
/// let trunk = TrunkId::new(NodeId::new(1), 0);
/// assert_eq!(trunk.change_id(), NodeId::new(1));
/// assert_eq!(trunk.file_idx(), 0);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TrunkId {
    change_id: NodeId,
    file_idx: u32,
}

impl TrunkId {
    /// Root trunk ID (repository root directory sentinel).
    pub const ROOT: TrunkId = TrunkId {
        change_id: NodeId::ROOT,
        file_idx: 0,
    };

    /// Creates a new [`TrunkId`].
    #[inline]
    pub const fn new(change_id: NodeId, file_idx: u32) -> Self {
        TrunkId { change_id, file_idx }
    }

    /// Returns the change that created this file.
    #[inline]
    pub const fn change_id(&self) -> NodeId {
        self.change_id
    }

    /// Returns the index within the creating change.
    #[inline]
    pub const fn file_idx(&self) -> u32 {
        self.file_idx
    }

    /// Returns `true` if this is the root trunk ID.
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Encodes as 12 bytes for storage.
    #[inline]
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[0..8].copy_from_slice(&self.change_id.get().to_le_bytes());
        bytes[8..12].copy_from_slice(&self.file_idx.to_le_bytes());
        bytes
    }

    /// Decodes from 12 bytes.
    #[inline]
    pub fn from_bytes(bytes: &[u8; 12]) -> Self {
        let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
        let file_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        TrunkId { change_id, file_idx }
    }
}

impl fmt::Debug for TrunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrunkId({}, {})", self.change_id.get(), self.file_idx)
    }
}

impl fmt::Display for TrunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "T{}:{}", self.change_id.get(), self.file_idx)
    }
}

// BranchId - Line Level Identifier

/// Unique identifier for a line (branch) in the CRDT graph.
///
/// Lines are the primary ordering unit within a file.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::BranchId;
/// use atomic_core::types::NodeId;
///
/// let branch = BranchId::new(NodeId::new(1), 0);
/// assert!(!branch.is_root());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BranchId {
    change_id: NodeId,
    branch_idx: u32,
}

impl BranchId {
    /// Root branch ID (start-of-file sentinel).
    pub const ROOT: BranchId = BranchId {
        change_id: NodeId::ROOT,
        branch_idx: 0,
    };

    /// Creates a new [`BranchId`].
    #[inline]
    pub const fn new(change_id: NodeId, branch_idx: u32) -> Self {
        BranchId { change_id, branch_idx }
    }

    /// Returns the change that created this line.
    #[inline]
    pub const fn change_id(&self) -> NodeId {
        self.change_id
    }

    /// Returns the index within the creating change.
    #[inline]
    pub const fn branch_idx(&self) -> u32 {
        self.branch_idx
    }

    /// Returns `true` if this is the root branch ID.
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Encodes as 12 bytes for storage.
    #[inline]
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[0..8].copy_from_slice(&self.change_id.get().to_le_bytes());
        bytes[8..12].copy_from_slice(&self.branch_idx.to_le_bytes());
        bytes
    }

    /// Decodes from 12 bytes.
    #[inline]
    pub fn from_bytes(bytes: &[u8; 12]) -> Self {
        let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
        let branch_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        BranchId { change_id, branch_idx }
    }
}

impl fmt::Debug for BranchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BranchId({}, {})", self.change_id.get(), self.branch_idx)
    }
}

impl fmt::Display for BranchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "B{}:{}", self.change_id.get(), self.branch_idx)
    }
}

// LeafId - Token Level Identifier

/// Unique identifier for a token (leaf) in the CRDT graph.
///
/// Tokens are the atomic units of content within a line.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::LeafId;
/// use atomic_core::types::NodeId;
///
/// let leaf = LeafId::new(NodeId::new(1), 0);
/// assert_eq!(leaf.leaf_idx(), 0);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LeafId {
    change_id: NodeId,
    leaf_idx: u32,
}

impl LeafId {
    /// Root leaf ID (start-of-line sentinel).
    pub const ROOT: LeafId = LeafId {
        change_id: NodeId::ROOT,
        leaf_idx: 0,
    };

    /// Creates a new [`LeafId`].
    #[inline]
    pub const fn new(change_id: NodeId, leaf_idx: u32) -> Self {
        LeafId { change_id, leaf_idx }
    }

    /// Returns the change that created this token.
    #[inline]
    pub const fn change_id(&self) -> NodeId {
        self.change_id
    }

    /// Returns the index within the creating change.
    #[inline]
    pub const fn leaf_idx(&self) -> u32 {
        self.leaf_idx
    }

    /// Returns `true` if this is the root leaf ID.
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Encodes as 12 bytes for storage.
    #[inline]
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[0..8].copy_from_slice(&self.change_id.get().to_le_bytes());
        bytes[8..12].copy_from_slice(&self.leaf_idx.to_le_bytes());
        bytes
    }

    /// Decodes from 12 bytes.
    #[inline]
    pub fn from_bytes(bytes: &[u8; 12]) -> Self {
        let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
        let leaf_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        LeafId { change_id, leaf_idx }
    }
}

impl fmt::Debug for LeafId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LeafId({}, {})", self.change_id.get(), self.leaf_idx)
    }
}

impl fmt::Display for LeafId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}:{}", self.change_id.get(), self.leaf_idx)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // TrunkId Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_id_new() {
        let id = TrunkId::new(NodeId::new(42), 5);
        assert_eq!(id.change_id(), NodeId::new(42));
        assert_eq!(id.file_idx(), 5);
    }

    #[test]
    fn test_trunk_id_root() {
        assert!(TrunkId::ROOT.is_root());
        assert!(!TrunkId::new(NodeId::new(1), 0).is_root());
    }

    #[test]
    fn test_trunk_id_bytes_roundtrip() {
        let id = TrunkId::new(NodeId::new(12345), 99);
        let bytes = id.to_bytes();
        let decoded = TrunkId::from_bytes(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_trunk_id_ordering() {
        let a = TrunkId::new(NodeId::new(1), 0);
        let b = TrunkId::new(NodeId::new(1), 1);
        let c = TrunkId::new(NodeId::new(2), 0);

        assert!(a < b, "same change, lower idx comes first");
        assert!(b < c, "lower change_id comes first");
    }

    #[test]
    fn test_trunk_id_display() {
        let id = TrunkId::new(NodeId::new(42), 3);
        assert_eq!(format!("{}", id), "T42:3");
        assert_eq!(format!("{:?}", id), "TrunkId(42, 3)");
    }

    #[test]
    fn test_trunk_id_serde() {
        let id = TrunkId::new(NodeId::new(100), 7);
        let json = serde_json::to_string(&id).unwrap();
        let decoded: TrunkId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    // -------------------------------------------------------------------------
    // BranchId Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_branch_id_new() {
        let id = BranchId::new(NodeId::new(42), 5);
        assert_eq!(id.change_id(), NodeId::new(42));
        assert_eq!(id.branch_idx(), 5);
    }

    #[test]
    fn test_branch_id_root() {
        assert!(BranchId::ROOT.is_root());
        assert!(!BranchId::new(NodeId::new(1), 0).is_root());
    }

    #[test]
    fn test_branch_id_bytes_roundtrip() {
        let id = BranchId::new(NodeId::new(12345), 99);
        let bytes = id.to_bytes();
        let decoded = BranchId::from_bytes(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_branch_id_ordering() {
        let a = BranchId::new(NodeId::new(1), 0);
        let b = BranchId::new(NodeId::new(1), 1);
        let c = BranchId::new(NodeId::new(2), 0);

        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn test_branch_id_display() {
        let id = BranchId::new(NodeId::new(42), 3);
        assert_eq!(format!("{}", id), "B42:3");
        assert_eq!(format!("{:?}", id), "BranchId(42, 3)");
    }

    #[test]
    fn test_branch_id_serde() {
        let id = BranchId::new(NodeId::new(100), 7);
        let json = serde_json::to_string(&id).unwrap();
        let decoded: BranchId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    // -------------------------------------------------------------------------
    // LeafId Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_leaf_id_new() {
        let id = LeafId::new(NodeId::new(42), 5);
        assert_eq!(id.change_id(), NodeId::new(42));
        assert_eq!(id.leaf_idx(), 5);
    }

    #[test]
    fn test_leaf_id_root() {
        assert!(LeafId::ROOT.is_root());
        assert!(!LeafId::new(NodeId::new(1), 0).is_root());
    }

    #[test]
    fn test_leaf_id_bytes_roundtrip() {
        let id = LeafId::new(NodeId::new(12345), 99);
        let bytes = id.to_bytes();
        let decoded = LeafId::from_bytes(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_leaf_id_ordering() {
        let a = LeafId::new(NodeId::new(1), 0);
        let b = LeafId::new(NodeId::new(1), 1);
        let c = LeafId::new(NodeId::new(2), 0);

        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn test_leaf_id_display() {
        let id = LeafId::new(NodeId::new(42), 3);
        assert_eq!(format!("{}", id), "L42:3");
        assert_eq!(format!("{:?}", id), "LeafId(42, 3)");
    }

    #[test]
    fn test_leaf_id_serde() {
        let id = LeafId::new(NodeId::new(100), 7);
        let json = serde_json::to_string(&id).unwrap();
        let decoded: LeafId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    // -------------------------------------------------------------------------
    // Cross-type Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_ids_same_change_different_types() {
        let change = NodeId::new(42);
        let trunk = TrunkId::new(change, 0);
        let branch = BranchId::new(change, 0);
        let leaf = LeafId::new(change, 0);

        assert_eq!(trunk.change_id(), change);
        assert_eq!(branch.change_id(), change);
        assert_eq!(leaf.change_id(), change);
    }

    #[test]
    fn test_ids_bytes_are_same_structure() {
        use std::collections::HashSet;

        let change = NodeId::new(1);
        let mut set = HashSet::new();

        let trunk_bytes = TrunkId::new(change, 0).to_bytes();
        let branch_bytes = BranchId::new(change, 0).to_bytes();
        let leaf_bytes = LeafId::new(change, 0).to_bytes();

        set.insert(trunk_bytes);
        set.insert(branch_bytes);
        set.insert(leaf_bytes);

        // All bytes are the same - type system distinguishes them
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_trunk_id_clone() {
        let id = TrunkId::new(NodeId::new(1), 0);
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    #[test]
    fn test_branch_id_hash() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        let id = BranchId::new(NodeId::new(1), 0);
        map.insert(id, "test");
        assert_eq!(map.get(&id), Some(&"test"));
    }

    #[test]
    fn test_leaf_id_eq() {
        let a = LeafId::new(NodeId::new(1), 0);
        let b = LeafId::new(NodeId::new(1), 0);
        let c = LeafId::new(NodeId::new(1), 1);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}

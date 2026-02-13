//! Pristine table definitions for the hierarchical CRDT graph model.
//!
//! This module defines the redb tables used to store the Trunk → Branch → Leaf
//! hierarchy, along with encoding/decoding helpers for compact key/value storage.
//!
//! # Table Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────────┐
//! │                    CRDT Pristine Table Architecture                          │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │                                                                             │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │                         TRUNKS Table                                 │   │
//! │  │  Key: TrunkId (12 bytes)                                            │   │
//! │  │  Value: SerializedTrunk (variable)                                  │   │
//! │  │  Purpose: Store file metadata (inode, path, encoding, state)        │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │       │                                                                     │
//! │       │ 1:N relationship via TRUNK_BRANCHES                                │
//! │       ▼                                                                     │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │                        BRANCHES Table                                │   │
//! │  │  Key: BranchId (12 bytes)                                           │   │
//! │  │  Value: SerializedBranch (24 bytes fixed)                           │   │
//! │  │  Purpose: Store line metadata (trunk ref, state, content hash)      │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │       │                                                                     │
//! │       │ 1:N relationship via BRANCH_LEAVES                                 │
//! │       ▼                                                                     │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │                         LEAVES Table                                 │   │
//! │  │  Key: LeafId (12 bytes)                                             │   │
//! │  │  Value: SerializedLeaf (22 bytes fixed)                             │   │
//! │  │  Purpose: Store token metadata (branch ref, kind, content range)    │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │                                                                             │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │                    Ordering Tables (Multimap)                        │   │
//! │  │                                                                      │   │
//! │  │  TRUNK_BRANCHES: TrunkId → [BranchId] (ordered line list)           │   │
//! │  │  BRANCH_LEAVES:  BranchId → [LeafId] (ordered token list)           │   │
//! │  │                                                                      │   │
//! │  │  Purpose: Maintain CRDT ordering within parent containers           │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │                                                                             │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │                    Reverse Lookup Tables                             │   │
//! │  │                                                                      │   │
//! │  │  INODE_TRUNK: Inode → TrunkId (find file by inode)                  │   │
//! │  │  PATH_TRUNK:  Path → TrunkId (find file by path)                    │   │
//! │  │                                                                      │   │
//! │  │  Purpose: Efficient lookups from external identifiers               │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │                                                                             │
//! └─────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Encoding Strategy
//!
//! All CRDT IDs use a consistent 12-byte encoding:
//! - Bytes 0-7: `change_id` as little-endian u64
//! - Bytes 8-11: index (file_idx/branch_idx/leaf_idx) as little-endian u32
//!
//! This encoding ensures:
//! 1. **Deterministic ordering**: Keys sort by change_id first, then by index
//! 2. **CRDT conflict resolution**: Concurrent inserts are ordered by ID
//! 3. **Compact storage**: Fixed 12-byte keys for efficient B-tree operations
//!
//! # Value Encoding Strategy
//!
//! Values use fixed-size encodings where possible for predictable performance:
//!
//! | Table | Value Size | Contents |
//! |-------|------------|----------|
//! | BRANCHES | 24 bytes | trunk_id (12) + state (1) + padding (3) + line_hash (8) |
//! | LEAVES | 22 bytes | branch_id (12) + kind (1) + state (1) + content_range (8) |
//! | TRUNKS | Variable | Serialized with bincode (path is variable length) |
//!
//! # Example Usage
//!
//! ```rust,ignore
//! use atomic_core::crdt::tables::*;
//! use atomic_core::crdt::{TrunkId, BranchId, LeafId};
//! use atomic_core::types::NodeId;
//!
//! // Encode a TrunkId for storage
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//! let key = encode_trunk_id(&trunk_id);
//!
//! // Decode back
//! let decoded = decode_trunk_id(&key);
//! assert_eq!(trunk_id, decoded);
//! ```

use redb::{MultimapTableDefinition, TableDefinition};

use super::branch::BranchState;
use super::ids::{BranchId, LeafId, TrunkId};
use super::leaf::LeafState;
use super::trunk::TrunkState;
use crate::diff::token::TokenKind;
use crate::types::{Inode, NodeId};

// =============================================================================
// Table Definitions
// =============================================================================

/// Trunk (file) storage: TrunkId → SerializedTrunk
///
/// Key: 12 bytes encoding TrunkId (change_id: u64, file_idx: u32)
/// Value: Variable-length serialized Trunk data
///
/// Stores file metadata including inode, path, encoding, and lifecycle state.
/// The path is variable-length, so we use bincode serialization for values.
pub const TRUNKS: TableDefinition<&[u8; 12], &[u8]> = TableDefinition::new("crdt_trunks");

/// Branch (line) storage: BranchId → SerializedBranch
///
/// Key: 12 bytes encoding BranchId (change_id: u64, branch_idx: u32)
/// Value: 24 bytes encoding (trunk_id: 12, state: 1, padding: 3, line_hash: 8)
///
/// Stores line metadata with fixed-size encoding for efficient access.
pub const BRANCHES: TableDefinition<&[u8; 12], &[u8; 24]> = TableDefinition::new("crdt_branches");

/// Leaf (token) storage: LeafId → SerializedLeaf
///
/// Key: 12 bytes encoding LeafId (change_id: u64, leaf_idx: u32)
/// Value: 22 bytes encoding (branch_id: 12, kind: 1, state: 1, start: 4, end: 4)
///
/// Stores token metadata with fixed-size encoding for efficient access.
pub const LEAVES: TableDefinition<&[u8; 12], &[u8; 22]> = TableDefinition::new("crdt_leaves");

/// Trunk → Branches ordering: TrunkId → [BranchId] (multimap)
///
/// Key: 12 bytes encoding TrunkId
/// Value: 12 bytes encoding BranchId
///
/// Maintains the ordered list of lines within each file.
/// The multimap preserves insertion order, and BranchIds sort deterministically
/// for CRDT conflict resolution.
pub const TRUNK_BRANCHES: MultimapTableDefinition<&[u8; 12], &[u8; 12]> =
    MultimapTableDefinition::new("crdt_trunk_branches");

/// Branch → Leaves ordering: BranchId → [LeafId] (multimap)
///
/// Key: 12 bytes encoding BranchId
/// Value: 12 bytes encoding LeafId
///
/// Maintains the ordered list of tokens within each line.
/// LeafIds sort deterministically for CRDT conflict resolution.
pub const BRANCH_LEAVES: MultimapTableDefinition<&[u8; 12], &[u8; 12]> =
    MultimapTableDefinition::new("crdt_branch_leaves");

/// Inode → TrunkId reverse lookup
///
/// Key: Inode as u64
/// Value: 12 bytes encoding TrunkId
///
/// Enables finding a file's TrunkId from its stable inode identifier.
pub const INODE_TRUNK: TableDefinition<u64, &[u8; 12]> = TableDefinition::new("crdt_inode_trunk");

/// Path → TrunkId reverse lookup
///
/// Key: File path as string
/// Value: 12 bytes encoding TrunkId
///
/// Enables finding a file's TrunkId from its current path.
/// This must be updated when files are moved/renamed.
pub const PATH_TRUNK: TableDefinition<&str, &[u8; 12]> = TableDefinition::new("crdt_path_trunk");

/// BranchId → Graph Span position mapping
///
/// Key: 12 bytes encoding BranchId (change_id: u64, branch_idx: u32)
/// Value: 24 bytes encoding Span position (change_id: u64, start: u64, end: u64)
///
/// Maps CRDT branch identifiers to their corresponding graph span positions.
/// This enables finding the graph span when processing delete operations,
/// which is necessary to mark edges with DELETED flags.
///
/// # Layout (24 bytes)
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: span.change (NodeId as u64 LE)   │
/// │  Bytes 8-15: span.start (ChangePosition LE)  │
/// │  Bytes 16-23: span.end (ChangePosition LE)   │
/// └────────────────────────────────────────────────┘
/// ```
pub const BRANCH_VERTEX: TableDefinition<&[u8; 12], &[u8; 24]> =
    TableDefinition::new("crdt_branch_vertex");

// =============================================================================
// ID Encoding/Decoding (12 bytes each)
// =============================================================================

/// Encode a TrunkId as 12 bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: change_id (u64 LE)                 │
/// │  Bytes 8-11: file_idx (u32 LE)                 │
/// └────────────────────────────────────────────────┘
/// ```
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::tables::encode_trunk_id;
/// use atomic_core::crdt::TrunkId;
/// use atomic_core::types::NodeId;
///
/// let id = TrunkId::new(NodeId::new(42), 5);
/// let bytes = encode_trunk_id(&id);
/// assert_eq!(bytes.len(), 12);
/// ```
#[inline]
pub fn encode_trunk_id(id: &TrunkId) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0..8].copy_from_slice(&id.change_id().get().to_le_bytes());
    bytes[8..12].copy_from_slice(&id.file_idx().to_le_bytes());
    bytes
}

/// Decode a TrunkId from 12 bytes.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::tables::{encode_trunk_id, decode_trunk_id};
/// use atomic_core::crdt::TrunkId;
/// use atomic_core::types::NodeId;
///
/// let original = TrunkId::new(NodeId::new(42), 5);
/// let bytes = encode_trunk_id(&original);
/// let decoded = decode_trunk_id(&bytes);
/// assert_eq!(original, decoded);
/// ```
#[inline]
pub fn decode_trunk_id(bytes: &[u8; 12]) -> TrunkId {
    let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
    let file_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    TrunkId::new(change_id, file_idx)
}

/// Encode a BranchId as 12 bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: change_id (u64 LE)                 │
/// │  Bytes 8-11: branch_idx (u32 LE)               │
/// └────────────────────────────────────────────────┘
/// ```
#[inline]
pub fn encode_branch_id(id: &BranchId) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0..8].copy_from_slice(&id.change_id().get().to_le_bytes());
    bytes[8..12].copy_from_slice(&id.branch_idx().to_le_bytes());
    bytes
}

/// Decode a BranchId from 12 bytes.
#[inline]
pub fn decode_branch_id(bytes: &[u8; 12]) -> BranchId {
    let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
    let branch_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    BranchId::new(change_id, branch_idx)
}

/// Encode a LeafId as 12 bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: change_id (u64 LE)                 │
/// │  Bytes 8-11: leaf_idx (u32 LE)                 │
/// └────────────────────────────────────────────────┘
/// ```
#[inline]
pub fn encode_leaf_id(id: &LeafId) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0..8].copy_from_slice(&id.change_id().get().to_le_bytes());
    bytes[8..12].copy_from_slice(&id.leaf_idx().to_le_bytes());
    bytes
}

/// Decode a LeafId from 12 bytes.
#[inline]
pub fn decode_leaf_id(bytes: &[u8; 12]) -> LeafId {
    let change_id = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
    let leaf_idx = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    LeafId::new(change_id, leaf_idx)
}

// =============================================================================
// Span Position Encoding/Decoding (24 bytes)
// =============================================================================

use crate::types::{ChangePosition, GraphNode};

/// Encode a Span as 24 bytes for BRANCH_VERTEX storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: span.change (NodeId as u64 LE)   │
/// │  Bytes 8-15: span.start (ChangePosition LE)  │
/// │  Bytes 16-23: span.end (ChangePosition LE)   │
/// └────────────────────────────────────────────────┘
/// ```
#[inline]
pub fn encode_vertex_position(node: &GraphNode<NodeId>) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..8].copy_from_slice(&node.change.get().to_le_bytes());
    bytes[8..16].copy_from_slice(&node.start.get().to_le_bytes());
    bytes[16..24].copy_from_slice(&node.end.get().to_le_bytes());
    bytes
}

/// Decode a Span from 24 bytes.
#[inline]
pub fn decode_vertex_position(bytes: &[u8; 24]) -> GraphNode<NodeId> {
    let change = NodeId::new(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
    let start = ChangePosition::new(u64::from_le_bytes(bytes[8..16].try_into().unwrap()));
    let end = ChangePosition::new(u64::from_le_bytes(bytes[16..24].try_into().unwrap()));
    GraphNode { change, start, end }
}

// =============================================================================
// State Encoding/Decoding (1 byte each)
// =============================================================================

/// State byte constants for TrunkState.
pub mod trunk_state {
    /// Trunk is alive (visible in working copy).
    pub const ALIVE: u8 = 0;
    /// Trunk is deleted (hidden but restorable).
    pub const DELETED: u8 = 1;
    /// Trunk is a zombie (deleted but has live references).
    pub const ZOMBIE: u8 = 2;
}

/// Encode a TrunkState as a single byte.
#[inline]
pub fn encode_trunk_state(state: TrunkState) -> u8 {
    match state {
        TrunkState::Alive => trunk_state::ALIVE,
        TrunkState::Deleted => trunk_state::DELETED,
        TrunkState::Zombie => trunk_state::ZOMBIE,
    }
}

/// Decode a TrunkState from a byte.
///
/// Returns `TrunkState::Alive` for unknown values (defensive).
#[inline]
pub fn decode_trunk_state(byte: u8) -> TrunkState {
    match byte {
        trunk_state::ALIVE => TrunkState::Alive,
        trunk_state::DELETED => TrunkState::Deleted,
        trunk_state::ZOMBIE => TrunkState::Zombie,
        _ => TrunkState::Alive, // Defensive default
    }
}

/// State byte constants for BranchState.
pub mod branch_state {
    /// Branch is alive (visible in file output).
    pub const ALIVE: u8 = 0;
    /// Branch is deleted (hidden but restorable).
    pub const DELETED: u8 = 1;
}

/// Encode a BranchState as a single byte.
#[inline]
pub fn encode_branch_state(state: BranchState) -> u8 {
    match state {
        BranchState::Alive => branch_state::ALIVE,
        BranchState::Deleted => branch_state::DELETED,
    }
}

/// Decode a BranchState from a byte.
#[inline]
pub fn decode_branch_state(byte: u8) -> BranchState {
    match byte {
        branch_state::DELETED => BranchState::Deleted,
        _ => BranchState::Alive, // Default to alive
    }
}

/// State byte constants for LeafState.
pub mod leaf_state {
    /// Leaf is alive (visible in line output).
    pub const ALIVE: u8 = 0;
    /// Leaf is deleted (hidden but restorable).
    pub const DELETED: u8 = 1;
}

/// Encode a LeafState as a single byte.
#[inline]
pub fn encode_leaf_state(state: LeafState) -> u8 {
    match state {
        LeafState::Alive => leaf_state::ALIVE,
        LeafState::Deleted => leaf_state::DELETED,
    }
}

/// Decode a LeafState from a byte.
#[inline]
pub fn decode_leaf_state(byte: u8) -> LeafState {
    match byte {
        leaf_state::DELETED => LeafState::Deleted,
        _ => LeafState::Alive,
    }
}

// =============================================================================
// TokenKind Encoding/Decoding (1 byte)
// =============================================================================

/// TokenKind byte constants.
pub mod token_kind {
    pub const WORD: u8 = 0;
    pub const WHITESPACE: u8 = 1;
    pub const OPERATOR: u8 = 2;
    pub const PUNCTUATION: u8 = 3;
    pub const STRING: u8 = 4;
    pub const NUMBER: u8 = 5;
    pub const COMMENT: u8 = 6;
    pub const NEWLINE: u8 = 7;
    pub const OTHER: u8 = 8;
}

/// Encode a TokenKind as a single byte.
#[inline]
pub fn encode_token_kind(kind: TokenKind) -> u8 {
    match kind {
        TokenKind::Word => token_kind::WORD,
        TokenKind::Whitespace => token_kind::WHITESPACE,
        TokenKind::Operator => token_kind::OPERATOR,
        TokenKind::Punctuation => token_kind::PUNCTUATION,
        TokenKind::String => token_kind::STRING,
        TokenKind::Number => token_kind::NUMBER,
        TokenKind::Comment => token_kind::COMMENT,
        TokenKind::Newline => token_kind::NEWLINE,
        TokenKind::Other => token_kind::OTHER,
    }
}

/// Decode a TokenKind from a byte.
///
/// Returns `TokenKind::Other` for unknown values.
#[inline]
pub fn decode_token_kind(byte: u8) -> TokenKind {
    match byte {
        token_kind::WORD => TokenKind::Word,
        token_kind::WHITESPACE => TokenKind::Whitespace,
        token_kind::OPERATOR => TokenKind::Operator,
        token_kind::PUNCTUATION => TokenKind::Punctuation,
        token_kind::STRING => TokenKind::String,
        token_kind::NUMBER => TokenKind::Number,
        token_kind::COMMENT => TokenKind::Comment,
        token_kind::NEWLINE => TokenKind::Newline,
        _ => TokenKind::Other,
    }
}

// =============================================================================
// Branch Value Encoding/Decoding (24 bytes)
// =============================================================================

/// Serialized branch data stored in BRANCHES table.
///
/// This is a helper struct for encoding/decoding branch values.
/// Use [`encode_branch_value`] and [`decode_branch_value`] for storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedBranch {
    /// The trunk (file) this branch belongs to.
    pub trunk_id: TrunkId,
    /// Current lifecycle state.
    pub state: BranchState,
    /// FNV-1a hash of line content for fast equality checks.
    pub line_hash: u64,
}

/// Encode branch data as 24 bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-11: trunk_id (TrunkId encoding)       │
/// │  Byte 12: state (BranchState encoding)         │
/// │  Bytes 13-15: padding (reserved, set to 0)     │
/// │  Bytes 16-23: line_hash (u64 LE)               │
/// └────────────────────────────────────────────────┘
/// ```
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::tables::{encode_branch_value, decode_branch_value, SerializedBranch};
/// use atomic_core::crdt::{TrunkId, BranchState};
/// use atomic_core::types::NodeId;
///
/// let data = SerializedBranch {
///     trunk_id: TrunkId::new(NodeId::new(1), 0),
///     state: BranchState::Alive,
///     line_hash: 0x123456789ABCDEF0,
/// };
///
/// let bytes = encode_branch_value(&data);
/// let decoded = decode_branch_value(&bytes);
/// assert_eq!(data, decoded);
/// ```
#[inline]
pub fn encode_branch_value(data: &SerializedBranch) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    let trunk_bytes = encode_trunk_id(&data.trunk_id);
    bytes[0..12].copy_from_slice(&trunk_bytes);
    bytes[12] = encode_branch_state(data.state);
    // bytes[13..16] are padding (already zeroed)
    bytes[16..24].copy_from_slice(&data.line_hash.to_le_bytes());
    bytes
}

/// Decode branch data from 24 bytes.
#[inline]
pub fn decode_branch_value(bytes: &[u8; 24]) -> SerializedBranch {
    let trunk_id = decode_trunk_id(bytes[0..12].try_into().unwrap());
    let state = decode_branch_state(bytes[12]);
    let line_hash = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    SerializedBranch {
        trunk_id,
        state,
        line_hash,
    }
}

// =============================================================================
// Leaf Value Encoding/Decoding (22 bytes)
// =============================================================================

/// Serialized leaf data stored in LEAVES table.
///
/// This is a helper struct for encoding/decoding leaf values.
/// Use [`encode_leaf_value`] and [`decode_leaf_value`] for storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedLeaf {
    /// The branch (line) this leaf belongs to.
    pub branch_id: BranchId,
    /// Token type classification.
    pub kind: TokenKind,
    /// Current lifecycle state.
    pub state: LeafState,
    /// Start byte offset in content blob.
    pub content_start: u32,
    /// End byte offset in content blob.
    pub content_end: u32,
}

impl SerializedLeaf {
    /// Returns the content byte range.
    #[inline]
    pub fn content_range(&self) -> std::ops::Range<u32> {
        self.content_start..self.content_end
    }

    /// Returns the content length in bytes.
    #[inline]
    pub fn content_len(&self) -> u32 {
        self.content_end - self.content_start
    }
}

/// Encode leaf data as 22 bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-11: branch_id (BranchId encoding)     │
/// │  Byte 12: kind (TokenKind encoding)            │
/// │  Byte 13: state (LeafState encoding)           │
/// │  Bytes 14-17: content_start (u32 LE)           │
/// │  Bytes 18-21: content_end (u32 LE)             │
/// └────────────────────────────────────────────────┘
/// ```
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::tables::{encode_leaf_value, decode_leaf_value, SerializedLeaf};
/// use atomic_core::crdt::{BranchId, LeafState};
/// use atomic_core::diff::token::TokenKind;
/// use atomic_core::types::NodeId;
///
/// let data = SerializedLeaf {
///     branch_id: BranchId::new(NodeId::new(1), 0),
///     kind: TokenKind::Word,
///     state: LeafState::Alive,
///     content_start: 0,
///     content_end: 5,
/// };
///
/// let bytes = encode_leaf_value(&data);
/// let decoded = decode_leaf_value(&bytes);
/// assert_eq!(data, decoded);
/// ```
#[inline]
pub fn encode_leaf_value(data: &SerializedLeaf) -> [u8; 22] {
    let mut bytes = [0u8; 22];
    let branch_bytes = encode_branch_id(&data.branch_id);
    bytes[0..12].copy_from_slice(&branch_bytes);
    bytes[12] = encode_token_kind(data.kind);
    bytes[13] = encode_leaf_state(data.state);
    bytes[14..18].copy_from_slice(&data.content_start.to_le_bytes());
    bytes[18..22].copy_from_slice(&data.content_end.to_le_bytes());
    bytes
}

/// Decode leaf data from 22 bytes.
#[inline]
pub fn decode_leaf_value(bytes: &[u8; 22]) -> SerializedLeaf {
    let branch_id = decode_branch_id(bytes[0..12].try_into().unwrap());
    let kind = decode_token_kind(bytes[12]);
    let state = decode_leaf_state(bytes[13]);
    let content_start = u32::from_le_bytes(bytes[14..18].try_into().unwrap());
    let content_end = u32::from_le_bytes(bytes[18..22].try_into().unwrap());
    SerializedLeaf {
        branch_id,
        kind,
        state,
        content_start,
        content_end,
    }
}

// =============================================================================
// Trunk Value Encoding/Decoding (Variable Length)
// =============================================================================

/// Serialized trunk data stored in TRUNKS table.
///
/// This is a helper struct for encoding/decoding trunk values.
/// Unlike Branch and Leaf, Trunk values are variable-length due to the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedTrunk {
    /// Stable filesystem reference.
    pub inode: Inode,
    /// Current lifecycle state.
    pub state: TrunkState,
    /// Text encoding (0 = unknown, 1 = UTF-8, 2 = UTF-16LE, 3 = UTF-16BE, 4 = Binary).
    pub encoding: u8,
    /// Current file path (relative to repository root).
    pub path: String,
}

/// Encoding byte constants.
pub mod encoding {
    /// Unknown encoding.
    pub const UNKNOWN: u8 = 0;
    /// UTF-8 text encoding.
    pub const UTF8: u8 = 1;
    /// UTF-16 Little Endian encoding.
    pub const UTF16_LE: u8 = 2;
    /// UTF-16 Big Endian encoding.
    pub const UTF16_BE: u8 = 3;
    /// Binary (non-text) content.
    pub const BINARY: u8 = 4;
}

/// Encode trunk data as variable-length bytes for storage.
///
/// # Layout
///
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  Bytes 0-7: inode (u64 LE)                     │
/// │  Byte 8: state (TrunkState encoding)           │
/// │  Byte 9: encoding (Encoding byte)              │
/// │  Bytes 10-13: path_len (u32 LE)                │
/// │  Bytes 14..: path (UTF-8 bytes)                │
/// └────────────────────────────────────────────────┘
/// ```
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::tables::{encode_trunk_value, decode_trunk_value, SerializedTrunk, encoding};
/// use atomic_core::crdt::TrunkState;
/// use atomic_core::types::Inode;
///
/// let data = SerializedTrunk {
///     inode: Inode::new(42),
///     state: TrunkState::Alive,
///     encoding: encoding::UTF8,
///     path: "src/main.rs".to_string(),
/// };
///
/// let bytes = encode_trunk_value(&data);
/// let decoded = decode_trunk_value(&bytes).unwrap();
/// assert_eq!(data, decoded);
/// ```
pub fn encode_trunk_value(data: &SerializedTrunk) -> Vec<u8> {
    let path_bytes = data.path.as_bytes();
    let path_len = path_bytes.len() as u32;

    let mut bytes = Vec::with_capacity(14 + path_bytes.len());

    // Inode (8 bytes)
    bytes.extend_from_slice(&data.inode.get().to_le_bytes());

    // State (1 byte)
    bytes.push(encode_trunk_state(data.state));

    // Encoding (1 byte)
    bytes.push(data.encoding);

    // Path length (4 bytes)
    bytes.extend_from_slice(&path_len.to_le_bytes());

    // Path bytes
    bytes.extend_from_slice(path_bytes);

    bytes
}

/// Decode trunk data from variable-length bytes.
///
/// Returns `None` if the bytes are invalid or too short.
pub fn decode_trunk_value(bytes: &[u8]) -> Option<SerializedTrunk> {
    // Minimum size: 8 (inode) + 1 (state) + 1 (encoding) + 4 (path_len) = 14
    if bytes.len() < 14 {
        return None;
    }

    let inode = Inode::new(u64::from_le_bytes(bytes[0..8].try_into().ok()?));
    let state = decode_trunk_state(bytes[8]);
    let encoding = bytes[9];
    let path_len = u32::from_le_bytes(bytes[10..14].try_into().ok()?) as usize;

    if bytes.len() < 14 + path_len {
        return None;
    }

    let path = String::from_utf8(bytes[14..14 + path_len].to_vec()).ok()?;

    Some(SerializedTrunk {
        inode,
        state,
        encoding,
        path,
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // ID Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_id_encoding_roundtrip() {
        let id = TrunkId::new(NodeId::new(12345), 99);
        let bytes = encode_trunk_id(&id);
        let decoded = decode_trunk_id(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_trunk_id_encoding_root() {
        let id = TrunkId::ROOT;
        let bytes = encode_trunk_id(&id);
        let decoded = decode_trunk_id(&bytes);
        assert_eq!(id, decoded);
        assert!(decoded.is_root());
    }

    #[test]
    fn test_trunk_id_encoding_ordering() {
        let id1 = TrunkId::new(NodeId::new(1), 0);
        let id2 = TrunkId::new(NodeId::new(1), 1);
        let id3 = TrunkId::new(NodeId::new(2), 0);

        let bytes1 = encode_trunk_id(&id1);
        let bytes2 = encode_trunk_id(&id2);
        let bytes3 = encode_trunk_id(&id3);

        assert!(bytes1 < bytes2, "same change, lower idx should sort first");
        assert!(bytes2 < bytes3, "lower change_id should sort first");
    }

    #[test]
    fn test_branch_id_encoding_roundtrip() {
        let id = BranchId::new(NodeId::new(12345), 99);
        let bytes = encode_branch_id(&id);
        let decoded = decode_branch_id(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_branch_id_encoding_root() {
        let id = BranchId::ROOT;
        let bytes = encode_branch_id(&id);
        let decoded = decode_branch_id(&bytes);
        assert_eq!(id, decoded);
        assert!(decoded.is_root());
    }

    #[test]
    fn test_branch_id_encoding_ordering() {
        let id1 = BranchId::new(NodeId::new(1), 0);
        let id2 = BranchId::new(NodeId::new(1), 1);
        let id3 = BranchId::new(NodeId::new(2), 0);

        let bytes1 = encode_branch_id(&id1);
        let bytes2 = encode_branch_id(&id2);
        let bytes3 = encode_branch_id(&id3);

        assert!(bytes1 < bytes2);
        assert!(bytes2 < bytes3);
    }

    #[test]
    fn test_leaf_id_encoding_roundtrip() {
        let id = LeafId::new(NodeId::new(12345), 99);
        let bytes = encode_leaf_id(&id);
        let decoded = decode_leaf_id(&bytes);
        assert_eq!(id, decoded);
    }

    #[test]
    fn test_leaf_id_encoding_root() {
        let id = LeafId::ROOT;
        let bytes = encode_leaf_id(&id);
        let decoded = decode_leaf_id(&bytes);
        assert_eq!(id, decoded);
        assert!(decoded.is_root());
    }

    #[test]
    fn test_leaf_id_encoding_ordering() {
        let id1 = LeafId::new(NodeId::new(1), 0);
        let id2 = LeafId::new(NodeId::new(1), 1);
        let id3 = LeafId::new(NodeId::new(2), 0);

        let bytes1 = encode_leaf_id(&id1);
        let bytes2 = encode_leaf_id(&id2);
        let bytes3 = encode_leaf_id(&id3);

        assert!(bytes1 < bytes2);
        assert!(bytes2 < bytes3);
    }

    // -------------------------------------------------------------------------
    // State Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_state_encoding_roundtrip() {
        assert_eq!(decode_trunk_state(encode_trunk_state(TrunkState::Alive)), TrunkState::Alive);
        assert_eq!(decode_trunk_state(encode_trunk_state(TrunkState::Deleted)), TrunkState::Deleted);
        assert_eq!(decode_trunk_state(encode_trunk_state(TrunkState::Zombie)), TrunkState::Zombie);
    }

    #[test]
    fn test_trunk_state_encoding_unknown_defaults_to_alive() {
        assert_eq!(decode_trunk_state(255), TrunkState::Alive);
        assert_eq!(decode_trunk_state(100), TrunkState::Alive);
    }

    #[test]
    fn test_branch_state_encoding_roundtrip() {
        assert_eq!(decode_branch_state(encode_branch_state(BranchState::Alive)), BranchState::Alive);
        assert_eq!(decode_branch_state(encode_branch_state(BranchState::Deleted)), BranchState::Deleted);
    }

    #[test]
    fn test_branch_state_encoding_unknown_defaults_to_alive() {
        assert_eq!(decode_branch_state(255), BranchState::Alive);
    }

    #[test]
    fn test_leaf_state_encoding_roundtrip() {
        assert_eq!(decode_leaf_state(encode_leaf_state(LeafState::Alive)), LeafState::Alive);
        assert_eq!(decode_leaf_state(encode_leaf_state(LeafState::Deleted)), LeafState::Deleted);
    }

    #[test]
    fn test_leaf_state_encoding_unknown_defaults_to_alive() {
        assert_eq!(decode_leaf_state(255), LeafState::Alive);
    }

    // -------------------------------------------------------------------------
    // TokenKind Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_token_kind_encoding_roundtrip() {
        let kinds = [
            TokenKind::Word,
            TokenKind::Whitespace,
            TokenKind::Operator,
            TokenKind::Punctuation,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Newline,
            TokenKind::Other,
        ];

        for kind in kinds {
            let encoded = encode_token_kind(kind);
            let decoded = decode_token_kind(encoded);
            assert_eq!(kind, decoded, "Failed for {:?}", kind);
        }
    }

    #[test]
    fn test_token_kind_encoding_unknown_defaults_to_other() {
        assert_eq!(decode_token_kind(255), TokenKind::Other);
        assert_eq!(decode_token_kind(100), TokenKind::Other);
    }

    // -------------------------------------------------------------------------
    // Branch Value Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_branch_value_encoding_roundtrip() {
        let data = SerializedBranch {
            trunk_id: TrunkId::new(NodeId::new(42), 5),
            state: BranchState::Alive,
            line_hash: 0x123456789ABCDEF0,
        };

        let bytes = encode_branch_value(&data);
        assert_eq!(bytes.len(), 24);

        let decoded = decode_branch_value(&bytes);
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_branch_value_encoding_deleted_state() {
        let data = SerializedBranch {
            trunk_id: TrunkId::new(NodeId::new(1), 0),
            state: BranchState::Deleted,
            line_hash: 0,
        };

        let bytes = encode_branch_value(&data);
        let decoded = decode_branch_value(&bytes);
        assert_eq!(data, decoded);
        assert!(decoded.state.is_deleted());
    }

    #[test]
    fn test_branch_value_encoding_max_values() {
        let data = SerializedBranch {
            trunk_id: TrunkId::new(NodeId::new(u64::MAX), u32::MAX),
            state: BranchState::Alive,
            line_hash: u64::MAX,
        };

        let bytes = encode_branch_value(&data);
        let decoded = decode_branch_value(&bytes);
        assert_eq!(data, decoded);
    }

    // -------------------------------------------------------------------------
    // Leaf Value Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_leaf_value_encoding_roundtrip() {
        let data = SerializedLeaf {
            branch_id: BranchId::new(NodeId::new(42), 5),
            kind: TokenKind::Word,
            state: LeafState::Alive,
            content_start: 100,
            content_end: 200,
        };

        let bytes = encode_leaf_value(&data);
        assert_eq!(bytes.len(), 22);

        let decoded = decode_leaf_value(&bytes);
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_leaf_value_encoding_all_token_kinds() {
        let kinds = [
            TokenKind::Word,
            TokenKind::Whitespace,
            TokenKind::Operator,
            TokenKind::Punctuation,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Newline,
            TokenKind::Other,
        ];

        for kind in kinds {
            let data = SerializedLeaf {
                branch_id: BranchId::new(NodeId::new(1), 0),
                kind,
                state: LeafState::Alive,
                content_start: 0,
                content_end: 10,
            };

            let bytes = encode_leaf_value(&data);
            let decoded = decode_leaf_value(&bytes);
            assert_eq!(data.kind, decoded.kind, "Failed for {:?}", kind);
        }
    }

    #[test]
    fn test_leaf_value_encoding_deleted_state() {
        let data = SerializedLeaf {
            branch_id: BranchId::new(NodeId::new(1), 0),
            kind: TokenKind::Word,
            state: LeafState::Deleted,
            content_start: 0,
            content_end: 5,
        };

        let bytes = encode_leaf_value(&data);
        let decoded = decode_leaf_value(&bytes);
        assert_eq!(data, decoded);
        assert!(decoded.state.is_deleted());
    }

    #[test]
    fn test_leaf_value_encoding_max_values() {
        let data = SerializedLeaf {
            branch_id: BranchId::new(NodeId::new(u64::MAX), u32::MAX),
            kind: TokenKind::Other,
            state: LeafState::Deleted,
            content_start: u32::MAX,
            content_end: u32::MAX,
        };

        let bytes = encode_leaf_value(&data);
        let decoded = decode_leaf_value(&bytes);
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_serialized_leaf_content_range() {
        let data = SerializedLeaf {
            branch_id: BranchId::new(NodeId::new(1), 0),
            kind: TokenKind::Word,
            state: LeafState::Alive,
            content_start: 10,
            content_end: 25,
        };

        assert_eq!(data.content_range(), 10..25);
        assert_eq!(data.content_len(), 15);
    }

    // -------------------------------------------------------------------------
    // Trunk Value Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_trunk_value_encoding_roundtrip() {
        let data = SerializedTrunk {
            inode: Inode::new(42),
            state: TrunkState::Alive,
            encoding: encoding::UTF8,
            path: "src/main.rs".to_string(),
        };

        let bytes = encode_trunk_value(&data);
        let decoded = decode_trunk_value(&bytes).expect("decode failed");
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_trunk_value_encoding_empty_path() {
        let data = SerializedTrunk {
            inode: Inode::new(1),
            state: TrunkState::Alive,
            encoding: encoding::UNKNOWN,
            path: String::new(),
        };

        let bytes = encode_trunk_value(&data);
        assert_eq!(bytes.len(), 14); // minimum size with empty path

        let decoded = decode_trunk_value(&bytes).expect("decode failed");
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_trunk_value_encoding_long_path() {
        let long_path = "a/".repeat(100) + "file.rs";
        let data = SerializedTrunk {
            inode: Inode::new(999),
            state: TrunkState::Alive,
            encoding: encoding::UTF8,
            path: long_path.clone(),
        };

        let bytes = encode_trunk_value(&data);
        let decoded = decode_trunk_value(&bytes).expect("decode failed");
        assert_eq!(data.path, decoded.path);
    }

    #[test]
    fn test_trunk_value_encoding_unicode_path() {
        let data = SerializedTrunk {
            inode: Inode::new(1),
            state: TrunkState::Alive,
            encoding: encoding::UTF8,
            path: "文档/日本語/файл.txt".to_string(),
        };

        let bytes = encode_trunk_value(&data);
        let decoded = decode_trunk_value(&bytes).expect("decode failed");
        assert_eq!(data.path, decoded.path);
    }

    #[test]
    fn test_trunk_value_encoding_all_states() {
        let states = [TrunkState::Alive, TrunkState::Deleted, TrunkState::Zombie];

        for state in states {
            let data = SerializedTrunk {
                inode: Inode::new(1),
                state,
                encoding: encoding::UTF8,
                path: "test.rs".to_string(),
            };

            let bytes = encode_trunk_value(&data);
            let decoded = decode_trunk_value(&bytes).expect("decode failed");
            assert_eq!(data.state, decoded.state, "Failed for {:?}", state);
        }
    }

    #[test]
    fn test_trunk_value_encoding_all_encodings() {
        let encodings = [
            encoding::UNKNOWN,
            encoding::UTF8,
            encoding::UTF16_LE,
            encoding::UTF16_BE,
            encoding::BINARY,
        ];

        for enc in encodings {
            let data = SerializedTrunk {
                inode: Inode::new(1),
                state: TrunkState::Alive,
                encoding: enc,
                path: "test.txt".to_string(),
            };

            let bytes = encode_trunk_value(&data);
            let decoded = decode_trunk_value(&bytes).expect("decode failed");
            assert_eq!(data.encoding, decoded.encoding, "Failed for encoding {}", enc);
        }
    }

    #[test]
    fn test_trunk_value_decode_too_short() {
        // Less than minimum 14 bytes
        assert!(decode_trunk_value(&[0u8; 13]).is_none());
        assert!(decode_trunk_value(&[0u8; 0]).is_none());
    }

    #[test]
    fn test_trunk_value_decode_truncated_path() {
        let data = SerializedTrunk {
            inode: Inode::new(1),
            state: TrunkState::Alive,
            encoding: encoding::UTF8,
            path: "test.rs".to_string(),
        };

        let bytes = encode_trunk_value(&data);

        // Truncate the path portion
        let truncated = &bytes[..bytes.len() - 3];
        assert!(decode_trunk_value(truncated).is_none());
    }

    #[test]
    fn test_trunk_value_decode_invalid_utf8_path() {
        // Manually construct bytes with invalid UTF-8 in path
        let mut bytes = vec![0u8; 14];
        // inode = 1
        bytes[0..8].copy_from_slice(&1u64.to_le_bytes());
        // state = Alive
        bytes[8] = trunk_state::ALIVE;
        // encoding = UTF8
        bytes[9] = encoding::UTF8;
        // path_len = 3
        bytes[10..14].copy_from_slice(&3u32.to_le_bytes());
        // Invalid UTF-8 sequence
        bytes.extend_from_slice(&[0xFF, 0xFE, 0xFD]);

        assert!(decode_trunk_value(&bytes).is_none());
    }

    // -------------------------------------------------------------------------
    // Cross-Type Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_id_bytes_are_same_size() {
        let trunk = encode_trunk_id(&TrunkId::new(NodeId::new(1), 0));
        let branch = encode_branch_id(&BranchId::new(NodeId::new(1), 0));
        let leaf = encode_leaf_id(&LeafId::new(NodeId::new(1), 0));

        assert_eq!(trunk.len(), 12);
        assert_eq!(branch.len(), 12);
        assert_eq!(leaf.len(), 12);
    }

    #[test]
    fn test_same_ids_produce_same_bytes() {
        // IDs with same components should produce identical bytes
        // (They're distinguished by type system, not encoding)
        let trunk = encode_trunk_id(&TrunkId::new(NodeId::new(42), 5));
        let branch = encode_branch_id(&BranchId::new(NodeId::new(42), 5));
        let leaf = encode_leaf_id(&LeafId::new(NodeId::new(42), 5));

        assert_eq!(trunk, branch);
        assert_eq!(branch, leaf);
    }

    #[test]
    fn test_encoding_is_little_endian() {
        let id = TrunkId::new(NodeId::new(0x0102030405060708), 0x0A0B0C0D);
        let bytes = encode_trunk_id(&id);

        // Verify little-endian encoding
        assert_eq!(bytes[0], 0x08); // LSB of change_id
        assert_eq!(bytes[7], 0x01); // MSB of change_id
        assert_eq!(bytes[8], 0x0D); // LSB of file_idx
        assert_eq!(bytes[11], 0x0A); // MSB of file_idx
    }
}

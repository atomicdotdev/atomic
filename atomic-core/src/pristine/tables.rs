//! Table definitions for the pristine database
//!
//! This module defines all the redb tables used to store the repository graph
//! and metadata. Tables are organized by function:
//!
//! - **ID Mappings**: `EXTERNAL`, `INTERNAL` - map between internal NodeIds and external Hashes
//! - **Graph**: `GRAPH`, `INODE_GRAPH` - store vertices and edges
//! - **Stacks**: `STACKS`, `STACK_CHANGES`, `REV_STACK_CHANGES` - stack metadata
//! - **File Tree**: `TREE`, `REV_TREE`, `INODES`, `REV_INODES`, `DIRECTORIES` - file system mappings
//! - **Dependencies**: `DEPS`, `REV_DEPS` - change dependency tracking
//! - **State**: `STATES`, `TAGS` - stack state and tag tracking

use redb::{MultimapTableDefinition, TableDefinition};

// =============================================================================
// ID Mapping Tables
// =============================================================================

/// Maps NodeId (u64) → Hash ([u8; 32])
///
/// Converts internal repository-local IDs to external content-addressed hashes.
/// Used when serializing changes or communicating with remotes.
pub const EXTERNAL: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("external");

/// Maps Hash ([u8; 32]) → NodeId (u64)
///
/// Converts external hashes to internal IDs.
/// Used when applying changes from remotes or loading from disk.
pub const INTERNAL: TableDefinition<&[u8; 32], u64> = TableDefinition::new("internal");

/// Maps NodeId → NodeType (u8)
///
/// Tracks what type of node each ID represents:
/// - 0: Change
/// - 1: Tag
pub const NODE_TYPES: TableDefinition<u64, u8> = TableDefinition::new("node_types");

/// Node type constants
pub mod node_type {
    pub const CHANGE: u8 = 0;
    pub const TAG: u8 = 1;
    pub const ATTESTATION: u8 = 2;
}

// =============================================================================
// Graph Tables
// =============================================================================

/// Main graph table: GraphNode → [GraphEdge] (multimap)
///
/// Key: 24 bytes encoding (change_id: u64, start: u64, end: u64)
/// Value: 24 bytes encoding SerializedGraphEdge
///
/// This is the primary graph storage. Each span can have multiple outgoing edges.
pub const GRAPH: MultimapTableDefinition<&[u8; 24], &[u8; 24]> =
    MultimapTableDefinition::new("graph");

/// Inode-scoped graph for O(n) file traversal
///
/// Key: 32 bytes encoding (inode: u64, change_id: u64, start: u64, end: u64)
/// Value: 24 bytes encoding SerializedGraphEdge
///
/// This secondary index allows efficient iteration over all vertices belonging
/// to a specific file (inode), enabling O(n) file reads where n is the file size.
pub const INODE_GRAPH: MultimapTableDefinition<&[u8; 32], &[u8; 24]> =
    MultimapTableDefinition::new("inode_graph");

// =============================================================================
// Stack Tables
// =============================================================================

/// Stack metadata
///
/// Key: stack name (string)
/// Value: serialized StackState (variable length)
///
/// Stores stack configuration and current state (merkle root, head change, etc.)
/// Stacks are views of the graph - they represent which changes have been applied
/// in what order, not forks of the underlying data.
pub const STACKS: TableDefinition<&str, &[u8]> = TableDefinition::new("stacks");

/// Stack change log: (stack_id, sequence) → change_id
///
/// Key: 16 bytes encoding (stack_id: u64, sequence: u64)
/// Value: change NodeId
///
/// Tracks the ordered sequence of changes applied to each stack.
pub const STACK_CHANGES: TableDefinition<&[u8; 16], u64> = TableDefinition::new("stack_changes");

/// Reverse change log: (stack_id, change_id) → sequence
///
/// Key: 16 bytes encoding (stack_id: u64, change_id: u64)
/// Value: sequence number
///
/// Allows looking up when a change was applied to a stack.
pub const REV_STACK_CHANGES: TableDefinition<&[u8; 16], u64> =
    TableDefinition::new("rev_stack_changes");

// =============================================================================
// File Tree Tables
// =============================================================================

/// File tree: path → inode
///
/// Key: file path (string, relative to repo root)
/// Value: inode ID
///
/// Maps file paths to their stable inode identifiers.
pub const TREE: TableDefinition<&str, u64> = TableDefinition::new("tree");

/// Reverse tree: inode → path
///
/// Key: inode ID
/// Value: file path (string)
///
/// Maps inodes back to their current path.
pub const REV_TREE: TableDefinition<u64, &str> = TableDefinition::new("rev_tree");

/// Inodes: inode → Position
///
/// Key: inode ID
/// Value: 16 bytes encoding Position (change_id: u64, pos: u64)
///
/// Maps inodes to their position in the graph (the root span of the file).
pub const INODES: TableDefinition<u64, &[u8; 16]> = TableDefinition::new("inodes");

/// Reverse inodes: Position → inode
///
/// Key: 16 bytes encoding Position
/// Value: inode ID
///
/// Maps graph positions back to their inode.
pub const REV_INODES: TableDefinition<&[u8; 16], u64> = TableDefinition::new("rev_inodes");

/// Directories marker table: inode → flags
///
/// Key: inode ID
/// Value: directory flags (u8)
///
/// Marks which inodes represent directories rather than files. This enables
/// explicit directory tracking as first-class citizens in the repository.
///
/// # Directory Flags
///
/// | Flag | Value | Description |
/// |------|-------|-------------|
/// | `DIR_EXPLICIT` | 0x01 | Directory was explicitly tracked |
/// | `DIR_EMPTY` | 0x02 | Directory has no tracked children |
///
/// # Why Track Directories?
///
/// Unlike Git (which uses `.keep` files), Atomic tracks directories explicitly:
///
/// 1. **Empty directories** can be versioned without workarounds
/// 2. **Directory metadata** (permissions, ownership) can be preserved
/// 3. **Cleaner graph** - no synthetic files cluttering the repository
/// 4. **Semantic correctness** - a directory IS a directory, not a file
///
/// # Example
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────────┐
/// │                    Directory Tracking Flow                          │
/// ├─────────────────────────────────────────────────────────────────────┤
/// │                                                                     │
/// │  $ atomic add --directory src/empty_module/                         │
/// │                                                                     │
/// │  TREE[src/empty_module] = 42                                        │
/// │  REV_TREE[42] = src/empty_module                                    │
/// │  DIRECTORIES[42] = DIR_EXPLICIT | DIR_EMPTY  (0x03)                │
/// │                                                                     │
/// │  When recording:                                                    │
/// │  → Creates GraphOp::DirAdd with FOLDER edge flag                      │
/// │  → Directory becomes first-class graph citizen                      │
/// │                                                                     │
/// └─────────────────────────────────────────────────────────────────────┘
/// ```
pub const DIRECTORIES: TableDefinition<u64, u8> = TableDefinition::new("directories");

/// Directory flag constants
pub mod directory_flags {
    /// Directory was explicitly tracked (not just implicitly via file paths).
    ///
    /// When set, this directory will be included in changes even if empty.
    pub const DIR_EXPLICIT: u8 = 0x01;

    /// Directory currently has no tracked children.
    ///
    /// This flag is updated dynamically as files are added/removed.
    pub const DIR_EMPTY: u8 = 0x02;

    /// Check if flags indicate an explicitly tracked directory.
    #[inline]
    pub const fn is_explicit(flags: u8) -> bool {
        flags & DIR_EXPLICIT != 0
    }

    /// Check if flags indicate an empty directory.
    #[inline]
    pub const fn is_empty(flags: u8) -> bool {
        flags & DIR_EMPTY != 0
    }

    /// Create flags for an explicitly tracked empty directory.
    #[inline]
    pub const fn explicit_empty() -> u8 {
        DIR_EXPLICIT | DIR_EMPTY
    }

    /// Create flags for an explicitly tracked non-empty directory.
    #[inline]
    pub const fn explicit_with_children() -> u8 {
        DIR_EXPLICIT
    }
}

// =============================================================================
// Dependency Tables
// =============================================================================

/// Dependencies: change_id → [dep_id] (multimap)
///
/// Key: change NodeId
/// Value: dependency NodeId
///
/// Tracks which changes a given change depends on.
pub const DEPS: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("deps");

/// Reverse dependencies: dep_id → [change_id] (multimap)
///
/// Key: dependency NodeId
/// Value: dependent change NodeId
///
/// Tracks which changes depend on a given change (reverse of DEPS).
pub const REV_DEPS: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("rev_deps");

// =============================================================================
// State Tables
// =============================================================================

/// Stack states: (stack_id, merkle) → sequence
///
/// Key: 40 bytes encoding (stack_id: u64, merkle: [u8; 32])
/// Value: sequence number when this state was reached
///
/// Allows looking up when a stack reached a particular state.
pub const STATES: TableDefinition<&[u8; 40], u64> = TableDefinition::new("states");

/// Stack tags: (stack_id, sequence) → merkle
///
/// Key: 16 bytes encoding (stack_id: u64, sequence: u64)
/// Value: merkle hash at that sequence
///
/// Stores tagged states (named snapshots) for stacks.
pub const TAGS: TableDefinition<&[u8; 16], &[u8; 32]> = TableDefinition::new("tags");

// =============================================================================
// Key Encoding Helpers
// =============================================================================

/// Encode a span as 24 bytes for use as a graph key
#[inline]
pub fn encode_vertex(change_id: u64, start: u64, end: u64) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[0..8].copy_from_slice(&change_id.to_le_bytes());
    key[8..16].copy_from_slice(&start.to_le_bytes());
    key[16..24].copy_from_slice(&end.to_le_bytes());
    key
}

/// Decode a span from 24 bytes
#[inline]
pub fn decode_vertex(key: &[u8; 24]) -> (u64, u64, u64) {
    let change_id = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let start = u64::from_le_bytes(key[8..16].try_into().unwrap());
    let end = u64::from_le_bytes(key[16..24].try_into().unwrap());
    (change_id, start, end)
}

/// Encode an inode-span as 32 bytes for use as an inode_graph key
#[inline]
pub fn encode_inode_vertex(inode: u64, change_id: u64, start: u64, end: u64) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[0..8].copy_from_slice(&inode.to_le_bytes());
    key[8..16].copy_from_slice(&change_id.to_le_bytes());
    key[16..24].copy_from_slice(&start.to_le_bytes());
    key[24..32].copy_from_slice(&end.to_le_bytes());
    key
}

/// Decode an inode-span from 32 bytes
#[inline]
pub fn decode_inode_vertex(key: &[u8; 32]) -> (u64, u64, u64, u64) {
    let inode = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let change_id = u64::from_le_bytes(key[8..16].try_into().unwrap());
    let start = u64::from_le_bytes(key[16..24].try_into().unwrap());
    let end = u64::from_le_bytes(key[24..32].try_into().unwrap());
    (inode, change_id, start, end)
}

/// Encode a position as 16 bytes
#[inline]
pub fn encode_position(change_id: u64, pos: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&change_id.to_le_bytes());
    key[8..16].copy_from_slice(&pos.to_le_bytes());
    key
}

/// Decode a position from 16 bytes
#[inline]
pub fn decode_position(key: &[u8; 16]) -> (u64, u64) {
    let change_id = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let pos = u64::from_le_bytes(key[8..16].try_into().unwrap());
    (change_id, pos)
}

/// Encode a stack-sequence pair as 16 bytes
#[inline]
pub fn encode_stack_seq(stack_id: u64, seq: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&stack_id.to_le_bytes());
    key[8..16].copy_from_slice(&seq.to_le_bytes());
    key
}

/// Decode a stack-sequence pair from 16 bytes
#[inline]
pub fn decode_stack_seq(key: &[u8; 16]) -> (u64, u64) {
    let stack_id = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let seq = u64::from_le_bytes(key[8..16].try_into().unwrap());
    (stack_id, seq)
}

/// Encode a stack-merkle pair as 40 bytes
#[inline]
pub fn encode_stack_merkle(stack_id: u64, merkle: &[u8; 32]) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[0..8].copy_from_slice(&stack_id.to_le_bytes());
    key[8..40].copy_from_slice(merkle);
    key
}

/// Decode a stack-merkle pair from 40 bytes
#[inline]
pub fn decode_stack_merkle(key: &[u8; 40]) -> (u64, [u8; 32]) {
    let stack_id = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let mut merkle = [0u8; 32];
    merkle.copy_from_slice(&key[8..40]);
    (stack_id, merkle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vertex_encoding_roundtrip() {
        let change_id = 12345u64;
        let start = 100u64;
        let end = 200u64;

        let encoded = encode_vertex(change_id, start, end);
        let (dec_change, dec_start, dec_end) = decode_vertex(&encoded);

        assert_eq!(dec_change, change_id);
        assert_eq!(dec_start, start);
        assert_eq!(dec_end, end);
    }

    #[test]
    fn test_inode_vertex_encoding_roundtrip() {
        let inode = 42u64;
        let change_id = 12345u64;
        let start = 100u64;
        let end = 200u64;

        let encoded = encode_inode_vertex(inode, change_id, start, end);
        let (dec_inode, dec_change, dec_start, dec_end) = decode_inode_vertex(&encoded);

        assert_eq!(dec_inode, inode);
        assert_eq!(dec_change, change_id);
        assert_eq!(dec_start, start);
        assert_eq!(dec_end, end);
    }

    #[test]
    fn test_position_encoding_roundtrip() {
        let change_id = 999u64;
        let pos = 12345u64;

        let encoded = encode_position(change_id, pos);
        let (dec_change, dec_pos) = decode_position(&encoded);

        assert_eq!(dec_change, change_id);
        assert_eq!(dec_pos, pos);
    }

    #[test]
    fn test_stack_seq_encoding_roundtrip() {
        let stack_id = 1u64;
        let seq = 42u64;

        let encoded = encode_stack_seq(stack_id, seq);
        let (dec_stack, dec_seq) = decode_stack_seq(&encoded);

        assert_eq!(dec_stack, stack_id);
        assert_eq!(dec_seq, seq);
    }

    #[test]
    fn test_stack_merkle_encoding_roundtrip() {
        let stack_id = 5u64;
        let merkle = [0xABu8; 32];

        let encoded = encode_stack_merkle(stack_id, &merkle);
        let (dec_stack, dec_merkle) = decode_stack_merkle(&encoded);

        assert_eq!(dec_stack, stack_id);
        assert_eq!(dec_merkle, merkle);
    }

    #[test]
    fn test_vertex_encoding_ordering() {
        // Verify that encoded vertices sort correctly (by change_id, then start, then end)
        let v1 = encode_vertex(1, 0, 10);
        let v2 = encode_vertex(1, 10, 20);
        let v3 = encode_vertex(2, 0, 10);

        assert!(v1 < v2, "same change, v1 should be before v2");
        assert!(v2 < v3, "v2 should be before v3 (different change)");
    }

    #[test]
    fn test_inode_vertex_encoding_ordering() {
        // Verify that inode-vertices sort by inode first
        let v1 = encode_inode_vertex(1, 2, 0, 10);
        let v2 = encode_inode_vertex(2, 1, 0, 10);

        assert!(v1 < v2, "inode 1 should sort before inode 2");
    }

    // =========================================================================
    // Directory Flag Tests
    // =========================================================================

    #[test]
    fn test_directory_flags_explicit() {
        use super::directory_flags::*;

        assert!(is_explicit(DIR_EXPLICIT));
        assert!(is_explicit(DIR_EXPLICIT | DIR_EMPTY));
        assert!(!is_explicit(0));
        assert!(!is_explicit(DIR_EMPTY));
    }

    #[test]
    fn test_directory_flags_empty() {
        use super::directory_flags::*;

        assert!(is_empty(DIR_EMPTY));
        assert!(is_empty(DIR_EXPLICIT | DIR_EMPTY));
        assert!(!is_empty(0));
        assert!(!is_empty(DIR_EXPLICIT));
    }

    #[test]
    fn test_directory_flags_explicit_empty() {
        use super::directory_flags::*;

        let flags = explicit_empty();
        assert!(is_explicit(flags));
        assert!(is_empty(flags));
        assert_eq!(flags, DIR_EXPLICIT | DIR_EMPTY);
    }

    #[test]
    fn test_directory_flags_explicit_with_children() {
        use super::directory_flags::*;

        let flags = explicit_with_children();
        assert!(is_explicit(flags));
        assert!(!is_empty(flags));
        assert_eq!(flags, DIR_EXPLICIT);
    }

    #[test]
    fn test_directory_flags_combinations() {
        use super::directory_flags::*;

        // Test all possible flag combinations
        assert!(!is_explicit(0b00));
        assert!(!is_empty(0b00));

        assert!(is_explicit(0b01));
        assert!(!is_empty(0b01));

        assert!(!is_explicit(0b10));
        assert!(is_empty(0b10));

        assert!(is_explicit(0b11));
        assert!(is_empty(0b11));
    }
}

//! Helper functions and types for pristine transactions
//!
//! This module contains serialization/deserialization helpers and
//! iterator types used by both read and write transactions.

#[allow(unused_imports)]
use crate::types::{
    ChangePosition, EdgeFlags, Hash, Merkle, NodeId, Position, SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::traits::StackState;

// =============================================================================
// Edge Serialization
// =============================================================================

/// Serialize a SerializedGraphEdge to bytes (24 bytes)
///
/// Layout: [flags:8 | pos:56][change_id:64][introduced_by:64]
#[inline]
pub fn serialize_edge(edge: &SerializedGraphEdge) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    let dest = edge.dest();
    let flag = edge.flag();
    let introduced_by = edge.introduced_by();

    // Pack flag into high byte of first u64, position in low 56 bits
    let flag_and_pos = ((flag.bits() as u64) << 56) | (dest.pos.get() & ((1 << 56) - 1));
    bytes[0..8].copy_from_slice(&flag_and_pos.to_le_bytes());
    bytes[8..16].copy_from_slice(&dest.change.get().to_le_bytes());
    bytes[16..24].copy_from_slice(&introduced_by.get().to_le_bytes());
    bytes
}

/// Deserialize bytes to a SerializedGraphEdge
#[inline]
pub fn deserialize_edge(bytes: &[u8; 24]) -> SerializedGraphEdge {
    let flag_and_pos = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let change_id = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let introduced_by = u64::from_le_bytes(bytes[16..24].try_into().unwrap());

    let flag = EdgeFlags::from_bits_truncate((flag_and_pos >> 56) as u8);
    let pos = flag_and_pos & ((1 << 56) - 1);

    let dest = Position::new(NodeId::new(change_id), ChangePosition::new(pos));
    SerializedGraphEdge::new(flag, dest, NodeId::new(introduced_by))
}

// =============================================================================
// Stack State Serialization
// =============================================================================

/// Serialize a StackState to bytes
///
/// Layout: [id:8][name_len:4][name:var][merkle:32][change_count:8]
pub fn serialize_stack_state(state: &StackState) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + 4 + state.name.len() + 32 + 8);
    // id: u64
    bytes.extend_from_slice(&state.id.to_le_bytes());
    // name length: u32
    bytes.extend_from_slice(&(state.name.len() as u32).to_le_bytes());
    // name bytes
    bytes.extend_from_slice(state.name.as_bytes());
    // merkle state: [u8; 32]
    bytes.extend_from_slice(state.state.as_bytes());
    // change_count: u64
    bytes.extend_from_slice(&state.change_count.to_le_bytes());
    bytes
}

/// Deserialize bytes to a StackState
pub fn deserialize_stack_state(bytes: &[u8]) -> PristineResult<StackState> {
    const MIN_SIZE: usize = 8 + 4; // id + name_len

    if bytes.len() < MIN_SIZE {
        return Err(PristineError::Serialization {
            message: "stack state too short".to_string(),
        });
    }

    let id = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let name_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;

    let expected_size = 12 + name_len + 32 + 8;
    if bytes.len() < expected_size {
        return Err(PristineError::Serialization {
            message: "stack state truncated".to_string(),
        });
    }

    let name = String::from_utf8(bytes[12..12 + name_len].to_vec()).map_err(|_| {
        PristineError::Serialization {
            message: "invalid stack name encoding".to_string(),
        }
    })?;

    let mut merkle_bytes = [0u8; 32];
    merkle_bytes.copy_from_slice(&bytes[12 + name_len..12 + name_len + 32]);
    let state = Merkle::from_bytes(merkle_bytes);

    let change_count = u64::from_le_bytes(
        bytes[12 + name_len + 32..12 + name_len + 40]
            .try_into()
            .unwrap(),
    );

    Ok(StackState {
        id,
        name,
        state,
        change_count,
    })
}

// =============================================================================
// Adjacency Iterator
// =============================================================================

/// Iterator over adjacent edges
///
/// This collects edges into a Vec to avoid lifetime issues with redb.
pub struct AdjIterator {
    edges: Vec<SerializedGraphEdge>,
    index: usize,
}

impl AdjIterator {
    /// Create a new adjacency iterator from a vector of edges
    pub fn new(edges: Vec<SerializedGraphEdge>) -> Self {
        Self { edges, index: 0 }
    }

    /// Create an empty iterator
    pub fn empty() -> Self {
        Self {
            edges: Vec::new(),
            index: 0,
        }
    }
}

impl Iterator for AdjIterator {
    type Item = Result<SerializedGraphEdge, PristineError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.edges.len() {
            let edge = self.edges[self.index];
            self.index += 1;
            Some(Ok(edge))
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.edges.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for AdjIterator {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deserialize_edge() {
        let dest = Position::new(NodeId::new(42), ChangePosition::new(12345));
        let edge =
            SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::PARENT, dest, NodeId::new(7));

        let bytes = serialize_edge(&edge);
        let recovered = deserialize_edge(&bytes);

        assert_eq!(edge.flag(), recovered.flag());
        assert_eq!(edge.dest().change, recovered.dest().change);
        assert_eq!(edge.dest().pos, recovered.dest().pos);
        assert_eq!(edge.introduced_by(), recovered.introduced_by());
    }

    #[test]
    fn test_edge_all_flags() {
        let dest = Position::new(NodeId::new(1), ChangePosition::new(0));

        for flag_bits in 0..=255u8 {
            let flag = EdgeFlags::from_bits_truncate(flag_bits);
            let edge = SerializedGraphEdge::new(flag, dest, NodeId::new(1));

            let bytes = serialize_edge(&edge);
            let recovered = deserialize_edge(&bytes);

            assert_eq!(edge.flag(), recovered.flag());
        }
    }

    #[test]
    fn test_edge_max_position() {
        let max_pos = (1u64 << 56) - 1;
        let dest = Position::new(NodeId::new(u64::MAX), ChangePosition::new(max_pos));
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(u64::MAX));

        let bytes = serialize_edge(&edge);
        let recovered = deserialize_edge(&bytes);

        assert_eq!(recovered.dest().pos.get(), max_pos);
        assert_eq!(recovered.dest().change.get(), u64::MAX);
        assert_eq!(recovered.introduced_by().get(), u64::MAX);
    }

    #[test]
    fn test_serialize_deserialize_stack_state() {
        let state = StackState {
            id: 42,
            name: "test-stack".to_string(),
            state: Hash::of(b"test state"),
            change_count: 100,
        };

        let bytes = serialize_stack_state(&state);
        let recovered = deserialize_stack_state(&bytes).unwrap();

        assert_eq!(state.id, recovered.id);
        assert_eq!(state.name, recovered.name);
        assert_eq!(state.state, recovered.state);
        assert_eq!(state.change_count, recovered.change_count);
    }

    #[test]
    fn test_stack_state_empty_name() {
        let state = StackState {
            id: 1,
            name: String::new(),
            state: Merkle::ZERO,
            change_count: 0,
        };

        let bytes = serialize_stack_state(&state);
        let recovered = deserialize_stack_state(&bytes).unwrap();

        assert_eq!(state.name, recovered.name);
    }

    #[test]
    fn test_stack_state_unicode_name() {
        let state = StackState {
            id: 1,
            name: "スタック-日本語".to_string(),
            state: Merkle::ZERO,
            change_count: 0,
        };

        let bytes = serialize_stack_state(&state);
        let recovered = deserialize_stack_state(&bytes).unwrap();

        assert_eq!(state.name, recovered.name);
    }

    #[test]
    fn test_stack_state_too_short() {
        let bytes = [0u8; 4]; // Too short
        let result = deserialize_stack_state(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_stack_state_truncated() {
        let state = StackState {
            id: 1,
            name: "test".to_string(),
            state: Merkle::ZERO,
            change_count: 0,
        };

        let bytes = serialize_stack_state(&state);
        let truncated = &bytes[..bytes.len() - 10]; // Cut off some bytes
        let result = deserialize_stack_state(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_adj_iterator() {
        let dest = Position::new(NodeId::new(1), ChangePosition::new(0));
        let edges = vec![
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, NodeId::new(1)),
            SerializedGraphEdge::new(EdgeFlags::FOLDER, dest, NodeId::new(2)),
            SerializedGraphEdge::new(EdgeFlags::PARENT, dest, NodeId::new(3)),
        ];

        let mut iter = AdjIterator::new(edges.clone());

        assert_eq!(iter.len(), 3);

        for (i, result) in iter.by_ref().enumerate() {
            let edge = result.unwrap();
            assert_eq!(edge.introduced_by().get(), (i + 1) as u64);
        }

        assert_eq!(iter.len(), 0);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_adj_iterator_empty() {
        let mut iter = AdjIterator::empty();
        assert_eq!(iter.len(), 0);
        assert!(iter.next().is_none());
    }
}

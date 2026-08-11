//! Helper functions and types for pristine transactions
//!
//! This module contains serialization/deserialization helpers and
//! iterator types used by both read and write transactions.

#[allow(unused_imports)]
use crate::types::{
    ChangePosition, EdgeFlags, Hash, Merkle, NodeId, Position, SerializedGraphEdge,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::traits::{StoredConflict, ViewScope, ViewState};

// Conflict Serialization

/// Serialize a list of conflicts to bytes (JSON).
///
/// Conflict metadata is tiny and rare, so JSON via `serde_json` (already a
/// dependency) is preferred over a hand-rolled binary format for robustness.
pub fn serialize_conflicts(conflicts: &[StoredConflict]) -> PristineResult<Vec<u8>> {
    serde_json::to_vec(conflicts).map_err(|e| PristineError::Serialization {
        message: format!("failed to serialize conflicts: {e}"),
    })
}

/// Deserialize bytes (JSON) to a list of conflicts.
pub fn deserialize_conflicts(bytes: &[u8]) -> PristineResult<Vec<StoredConflict>> {
    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
        message: format!("failed to deserialize conflicts: {e}"),
    })
}

// Edge Serialization

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

// View State Serialization

/// Sentinel value for "no parent" in serialized form.
///
/// We use `u64::MAX` because view IDs are allocated from an incrementing
/// counter starting at 1, so `u64::MAX` will never be a valid view ID.
const NO_PARENT: u64 = u64::MAX;

/// Serialize a ViewState to bytes
///
/// Layout (v2): [id:8][name_len:4][name:var][merkle:32][change_count:8][kind:1][parent:8]
///
/// The `kind` and `parent` fields are appended after the original layout,
/// making v1 data readable with backward-compatible defaults (Shared, no parent).
pub fn serialize_view_state(state: &ViewState) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + 4 + state.name.len() + 32 + 8 + 1 + 8);
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
    // kind: u8 (v2)
    bytes.push(state.kind as u8);
    // parent: u64 (v2) — NO_PARENT sentinel for None
    let parent_val = state.parent.unwrap_or(NO_PARENT);
    bytes.extend_from_slice(&parent_val.to_le_bytes());
    bytes
}

/// Deserialize bytes to a ViewState
///
/// Backward-compatible: if the v2 fields (`kind`, `parent`) are missing
/// (i.e., the data was written by an older version), defaults to
/// `ViewScope::Shared` and `parent: None`.
pub fn deserialize_view_state(bytes: &[u8]) -> PristineResult<ViewState> {
    const MIN_SIZE: usize = 8 + 4; // id + name_len

    if bytes.len() < MIN_SIZE {
        return Err(PristineError::Serialization {
            message: "view state too short".to_string(),
        });
    }

    let id = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let name_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;

    // v1 size: header(12) + name + merkle(32) + change_count(8)
    let v1_size = 12 + name_len + 32 + 8;
    if bytes.len() < v1_size {
        return Err(PristineError::Serialization {
            message: "view state truncated".to_string(),
        });
    }

    let name = String::from_utf8(bytes[12..12 + name_len].to_vec()).map_err(|_| {
        PristineError::Serialization {
            message: "invalid view name encoding".to_string(),
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

    // v2 fields: kind(1) + parent(8) = 9 additional bytes
    let v2_size = v1_size + 1 + 8;
    let (kind, parent) = if bytes.len() >= v2_size {
        let kind_byte = bytes[v1_size];
        let kind = ViewScope::from_u8(kind_byte).ok_or_else(|| PristineError::Serialization {
            message: format!("invalid view kind: {}", kind_byte),
        })?;

        let parent_val = u64::from_le_bytes(bytes[v1_size + 1..v1_size + 9].try_into().unwrap());
        let parent = if parent_val == NO_PARENT {
            None
        } else {
            Some(parent_val)
        };

        (kind, parent)
    } else {
        // v1 data — default to Shared with no parent
        (ViewScope::Shared, None)
    };

    Ok(ViewState {
        id,
        name,
        state,
        change_count,
        kind,
        parent,
    })
}

// Adjacency Iterator

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

// Tests

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
    fn test_serialize_deserialize_view_state() {
        let state = ViewState {
            id: 42,
            name: "test-view".to_string(),
            state: Hash::of(b"test state"),
            change_count: 100,
            kind: ViewScope::Shared,
            parent: None,
        };

        let bytes = serialize_view_state(&state);
        let recovered = deserialize_view_state(&bytes).unwrap();

        assert_eq!(state.id, recovered.id);
        assert_eq!(state.name, recovered.name);
        assert_eq!(state.state, recovered.state);
        assert_eq!(state.change_count, recovered.change_count);
        assert_eq!(state.kind, recovered.kind);
        assert_eq!(state.parent, recovered.parent);
    }

    #[test]
    fn test_serialize_deserialize_draft_with_parent() {
        let state = ViewState {
            id: 5,
            name: "feature-login".to_string(),
            state: Hash::of(b"some state"),
            change_count: 7,
            kind: ViewScope::Draft,
            parent: Some(2),
        };

        let bytes = serialize_view_state(&state);
        let recovered = deserialize_view_state(&bytes).unwrap();

        assert_eq!(recovered.kind, ViewScope::Draft);
        assert_eq!(recovered.parent, Some(2));
        assert_eq!(recovered.name, "feature-login");
        assert_eq!(recovered.change_count, 7);
    }

    #[test]
    fn test_deserialize_v1_backward_compatible() {
        // Simulate v1 format: [id:8][name_len:4][name:var][merkle:32][change_count:8]
        // Without the kind and parent fields
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&42u64.to_le_bytes()); // id
        let name = "old-view";
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes()); // name_len
        bytes.extend_from_slice(name.as_bytes()); // name
        bytes.extend_from_slice(&[0u8; 32]); // merkle (zero)
        bytes.extend_from_slice(&10u64.to_le_bytes()); // change_count

        let recovered = deserialize_view_state(&bytes).unwrap();

        assert_eq!(recovered.id, 42);
        assert_eq!(recovered.name, "old-view");
        assert_eq!(recovered.change_count, 10);
        // v1 data defaults to Shared + no parent
        assert_eq!(recovered.kind, ViewScope::Shared);
        assert_eq!(recovered.parent, None);
    }

    #[test]
    fn test_view_state_empty_name() {
        let state = ViewState {
            id: 1,
            name: String::new(),
            state: Merkle::ZERO,
            change_count: 0,
            kind: ViewScope::Shared,
            parent: None,
        };

        let bytes = serialize_view_state(&state);
        let recovered = deserialize_view_state(&bytes).unwrap();

        assert_eq!(state.name, recovered.name);
    }

    #[test]
    fn test_view_state_unicode_name() {
        let state = ViewState {
            id: 1,
            name: "ビュー-日本語".to_string(),
            state: Merkle::ZERO,
            change_count: 0,
            kind: ViewScope::Draft,
            parent: Some(99),
        };

        let bytes = serialize_view_state(&state);
        let recovered = deserialize_view_state(&bytes).unwrap();

        assert_eq!(state.name, recovered.name);
        assert_eq!(recovered.kind, ViewScope::Draft);
        assert_eq!(recovered.parent, Some(99));
    }

    #[test]
    fn test_view_state_too_short() {
        let bytes = [0u8; 4]; // Too short
        let result = deserialize_view_state(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_view_state_truncated() {
        let state = ViewState {
            id: 1,
            name: "test".to_string(),
            state: Merkle::ZERO,
            change_count: 0,
            kind: ViewScope::Shared,
            parent: None,
        };

        let bytes = serialize_view_state(&state);
        // Cut into the v1 portion (not just the v2 extension)
        let truncated = &bytes[..12];
        let result = deserialize_view_state(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_view_state_invalid_kind() {
        // Build valid v2 bytes but with an invalid kind byte
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes()); // id
        let name = "test";
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&[0u8; 32]); // merkle
        bytes.extend_from_slice(&0u64.to_le_bytes()); // change_count
        bytes.push(99); // invalid kind
        bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // parent (none)

        let result = deserialize_view_state(&bytes);
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

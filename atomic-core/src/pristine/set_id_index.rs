//! Versioned persisted index for order-invariant view identities.
//!
//! The index is derived data. Missing rows (including repositories created
//! before this table existed) are safe and mean "rebuild required". Unknown or
//! malformed rows fail closed rather than being mistaken for current data.

use crate::types::{Merkle, SetId};

use super::{PristineError, PristineResult};

/// Current codec version for [`SetIdIndexEntry`].
pub const SET_ID_INDEX_VERSION: u8 = 1;

/// Canonical byte width of a V1 index value.
pub const SET_ID_INDEX_V1_SIZE: usize = 1 + Merkle::SIZE + SetId::SIZE + 8;

/// Persisted projection identity for one view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetIdIndexEntry {
    /// Order-sensitive view identity when the row was produced.
    pub merkle: Merkle,
    /// Order-invariant identity of the effective projection closure.
    pub set_id: SetId,
    /// Number of changes in that validated closure.
    pub closure_len: u64,
}

/// Encode one index row without changing the SetId V1 bytes.
pub fn encode_set_id_index_entry(entry: SetIdIndexEntry) -> [u8; SET_ID_INDEX_V1_SIZE] {
    let mut bytes = [0; SET_ID_INDEX_V1_SIZE];
    bytes[0] = SET_ID_INDEX_VERSION;
    bytes[1..33].copy_from_slice(entry.merkle.as_bytes());
    bytes[33..65].copy_from_slice(entry.set_id.as_bytes());
    bytes[65..73].copy_from_slice(&entry.closure_len.to_le_bytes());
    bytes
}

/// Decode one index row, rejecting unknown versions and malformed bytes.
pub fn decode_set_id_index_entry(bytes: &[u8]) -> PristineResult<SetIdIndexEntry> {
    if bytes.len() != SET_ID_INDEX_V1_SIZE {
        return Err(PristineError::Serialization {
            message: format!(
                "invalid SetId index row length: expected {SET_ID_INDEX_V1_SIZE}, found {}",
                bytes.len()
            ),
        });
    }
    if bytes[0] != SET_ID_INDEX_VERSION {
        return Err(PristineError::Serialization {
            message: format!(
                "unsupported SetId index version {}; rebuild with a compatible Atomic binary",
                bytes[0]
            ),
        });
    }

    let mut merkle = [0; Merkle::SIZE];
    merkle.copy_from_slice(&bytes[1..33]);
    let mut set_id = [0; SetId::SIZE];
    set_id.copy_from_slice(&bytes[33..65]);
    let mut closure_len = [0; 8];
    closure_len.copy_from_slice(&bytes[65..73]);

    Ok(SetIdIndexEntry {
        merkle: Merkle::from_bytes(merkle),
        set_id: SetId::from_bytes(set_id),
        closure_len: u64::from_le_bytes(closure_len),
    })
}

/// Read access to the separately versioned SetId index.
pub trait SetIdIndexTxnT {
    /// Read the row for `view_id`, or `None` when the legacy index/table is absent.
    fn get_set_id_index(&self, view_id: u64) -> PristineResult<Option<SetIdIndexEntry>>;
}

/// Mutation access used by transactional refresh and rebuild.
pub trait SetIdIndexMutTxnT: SetIdIndexTxnT {
    /// Replace the derived row for `view_id`.
    fn put_set_id_index(&mut self, view_id: u64, entry: SetIdIndexEntry) -> PristineResult<()>;

    /// Remove every row so callers can rebuild from canonical view closures.
    fn clear_set_id_index(&mut self) -> PristineResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_codec_round_trips_exact_set_id_bytes() {
        let entry = SetIdIndexEntry {
            merkle: Merkle::from_bytes([7; 32]),
            set_id: SetId::from_bytes([11; 32]),
            closure_len: 42,
        };
        let bytes = encode_set_id_index_entry(entry);
        assert_eq!(&bytes[33..65], entry.set_id.as_bytes());
        assert_eq!(decode_set_id_index_entry(&bytes).unwrap(), entry);
    }

    #[test]
    fn decoder_rejects_unknown_versions() {
        let mut bytes = encode_set_id_index_entry(SetIdIndexEntry {
            merkle: Merkle::ZERO,
            set_id: SetId::ZERO,
            closure_len: 0,
        });
        bytes[0] = SET_ID_INDEX_VERSION + 1;
        assert!(decode_set_id_index_entry(&bytes).is_err());
    }
}

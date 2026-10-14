//! Persistent working-copy records and transaction traits.

use crate::pristine::error::{PristineError, PristineResult};
use crate::types::{Hash, Merkle, WorkingCopyId};

/// Current canonical encoding version for [`WorkingCopyRecord`].
pub const WORKING_COPY_RECORD_VERSION: u8 = 1;

/// Fixed encoded width of a V1 [`WorkingCopyRecord`].
pub const WORKING_COPY_RECORD_V1_SIZE: usize = 155;

const VERSION_OFFSET: usize = 0;
const ID_OFFSET: usize = 1;
const LOCATION_OFFSET: usize = ID_OFFSET + WorkingCopyId::SIZE;
const DESIRED_VIEW_OFFSET: usize = LOCATION_OFFSET + 32;
const DESIRED_STATE_OFFSET: usize = DESIRED_VIEW_OFFSET + 8;
const MATERIALIZED_TAG_OFFSET: usize = DESIRED_STATE_OFFSET + 32;
const MATERIALIZED_STATE_OFFSET: usize = MATERIALIZED_TAG_OFFSET + 1;
const MANIFEST_TAG_OFFSET: usize = MATERIALIZED_STATE_OFFSET + 32;
const MANIFEST_OFFSET: usize = MANIFEST_TAG_OFFSET + 1;

/// Durable state for one working directory or linked Git worktree.
///
/// `location_fingerprint` is a domain-separated hash of the canonical working
/// root and, when present, the resolved Git common directory, per-worktree Git
/// directory, and primary index path. It prevents copied identity files from
/// aliasing or overwriting the original working-copy record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopyRecord {
    /// Stable identity stored in the per-directory `working_copy_id` file.
    pub id: WorkingCopyId,
    /// Canonical filesystem and Git-administration identity fingerprint.
    pub location_fingerprint: Hash,
    /// Repository-local ID of the view this working copy should represent.
    pub desired_view: u64,
    /// Exact state of `desired_view` selected for this working copy.
    pub desired_state: Merkle,
    /// Last state known to have been materialized, if verified.
    pub materialized_state: Option<Merkle>,
    /// Manifest of the last materialized bytes, when available.
    pub materialized_manifest: Option<Hash>,
}

/// Encode a working-copy record using the canonical fixed-width V1 layout.
///
/// The table value remains variable-width so later record versions can choose a
/// different layout without changing the redb table type.
pub fn encode_working_copy_record(record: &WorkingCopyRecord) -> PristineResult<Vec<u8>> {
    if record.desired_view == 0 {
        return Err(working_copy_serialization_error(
            "desired view ID must be non-zero",
        ));
    }
    if record.materialized_manifest.is_some() && record.materialized_state.is_none() {
        return Err(working_copy_serialization_error(
            "materialized manifest requires a materialized state",
        ));
    }

    let mut bytes = vec![0u8; WORKING_COPY_RECORD_V1_SIZE];
    bytes[VERSION_OFFSET] = WORKING_COPY_RECORD_VERSION;
    bytes[ID_OFFSET..LOCATION_OFFSET].copy_from_slice(record.id.as_bytes());
    bytes[LOCATION_OFFSET..DESIRED_VIEW_OFFSET]
        .copy_from_slice(record.location_fingerprint.as_bytes());
    bytes[DESIRED_VIEW_OFFSET..DESIRED_STATE_OFFSET]
        .copy_from_slice(&record.desired_view.to_le_bytes());
    bytes[DESIRED_STATE_OFFSET..MATERIALIZED_TAG_OFFSET]
        .copy_from_slice(record.desired_state.as_bytes());

    if let Some(state) = record.materialized_state {
        bytes[MATERIALIZED_TAG_OFFSET] = 1;
        bytes[MATERIALIZED_STATE_OFFSET..MANIFEST_TAG_OFFSET].copy_from_slice(state.as_bytes());
    }
    if let Some(manifest) = record.materialized_manifest {
        bytes[MANIFEST_TAG_OFFSET] = 1;
        bytes[MANIFEST_OFFSET..WORKING_COPY_RECORD_V1_SIZE].copy_from_slice(manifest.as_bytes());
    }

    Ok(bytes)
}

/// Decode and validate one canonical versioned working-copy record.
pub fn decode_working_copy_record(bytes: &[u8]) -> PristineResult<WorkingCopyRecord> {
    let Some(version) = bytes.first().copied() else {
        return Err(working_copy_serialization_error(
            "working-copy record is empty",
        ));
    };
    if version != WORKING_COPY_RECORD_VERSION {
        return Err(working_copy_serialization_error(format!(
            "unsupported working-copy record version {version} (maximum supported version {WORKING_COPY_RECORD_VERSION})"
        )));
    }
    if bytes.len() != WORKING_COPY_RECORD_V1_SIZE {
        return Err(working_copy_serialization_error(format!(
            "working-copy record V1 has length {}, expected {WORKING_COPY_RECORD_V1_SIZE}",
            bytes.len()
        )));
    }

    let id = WorkingCopyId::from_bytes(
        bytes[ID_OFFSET..LOCATION_OFFSET]
            .try_into()
            .map_err(|_| working_copy_serialization_error("invalid working-copy ID bytes"))?,
    );
    let location_fingerprint = Hash::from_bytes(
        bytes[LOCATION_OFFSET..DESIRED_VIEW_OFFSET]
            .try_into()
            .map_err(|_| working_copy_serialization_error("invalid location fingerprint bytes"))?,
    );
    let desired_view = u64::from_le_bytes(
        bytes[DESIRED_VIEW_OFFSET..DESIRED_STATE_OFFSET]
            .try_into()
            .map_err(|_| working_copy_serialization_error("invalid desired view bytes"))?,
    );
    if desired_view == 0 {
        return Err(working_copy_serialization_error(
            "desired view ID must be non-zero",
        ));
    }
    let desired_state = Merkle::from_bytes(
        bytes[DESIRED_STATE_OFFSET..MATERIALIZED_TAG_OFFSET]
            .try_into()
            .map_err(|_| working_copy_serialization_error("invalid desired state bytes"))?,
    );
    let materialized_state = decode_optional_hash(
        bytes[MATERIALIZED_TAG_OFFSET],
        &bytes[MATERIALIZED_STATE_OFFSET..MANIFEST_TAG_OFFSET],
        "materialized state",
    )?;
    let materialized_manifest = decode_optional_hash(
        bytes[MANIFEST_TAG_OFFSET],
        &bytes[MANIFEST_OFFSET..WORKING_COPY_RECORD_V1_SIZE],
        "materialized manifest",
    )?;
    if materialized_manifest.is_some() && materialized_state.is_none() {
        return Err(working_copy_serialization_error(
            "materialized manifest requires a materialized state",
        ));
    }

    Ok(WorkingCopyRecord {
        id,
        location_fingerprint,
        desired_view,
        desired_state,
        materialized_state,
        materialized_manifest,
    })
}

fn decode_optional_hash(tag: u8, bytes: &[u8], field: &str) -> PristineResult<Option<Hash>> {
    match tag {
        0 => {
            if bytes.iter().any(|byte| *byte != 0) {
                return Err(working_copy_serialization_error(format!(
                    "absent {field} contains non-zero bytes"
                )));
            }
            Ok(None)
        }
        1 => Ok(Some(Hash::from_bytes(bytes.try_into().map_err(|_| {
            working_copy_serialization_error(format!("invalid {field} bytes"))
        })?))),
        other => Err(working_copy_serialization_error(format!(
            "invalid {field} presence tag {other}"
        ))),
    }
}

fn working_copy_serialization_error(message: impl Into<String>) -> PristineError {
    PristineError::Serialization {
        message: message.into(),
    }
}

/// Read access to persistent working-copy records.
pub trait WorkingCopyTxnT {
    /// Look up one working copy by its stable ULID.
    fn get_working_copy(
        &self,
        id: WorkingCopyId,
    ) -> Result<Option<WorkingCopyRecord>, PristineError>;

    /// Return every working-copy record in stable ID order.
    fn list_working_copies(&self) -> Result<Vec<WorkingCopyRecord>, PristineError>;

    /// Find the record bound to a canonical location fingerprint.
    fn find_working_copy_by_location(
        &self,
        location_fingerprint: &Hash,
    ) -> Result<Option<WorkingCopyRecord>, PristineError> {
        Ok(self
            .list_working_copies()?
            .into_iter()
            .find(|record| &record.location_fingerprint == location_fingerprint))
    }
}

/// Atomic mutation access to persistent working-copy records.
pub trait WorkingCopyMutTxnT: WorkingCopyTxnT {
    /// Insert or update a record without allowing ID/location aliasing.
    ///
    /// An existing ID may be updated only when its location fingerprint is
    /// unchanged. A location already bound to another ID is also rejected.
    fn put_working_copy(&mut self, record: &WorkingCopyRecord) -> Result<(), PristineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> WorkingCopyRecord {
        WorkingCopyRecord {
            id: WorkingCopyId::from_bytes([1; 16]),
            location_fingerprint: Hash::from_bytes([2; 32]),
            desired_view: 0x0102_0304_0506_0708,
            desired_state: Merkle::from_bytes([3; 32]),
            materialized_state: Some(Merkle::from_bytes([4; 32])),
            materialized_manifest: Some(Hash::from_bytes([5; 32])),
        }
    }

    #[test]
    fn working_copy_record_v1_has_stable_layout_and_roundtrips() {
        let record = record();
        let encoded = encode_working_copy_record(&record).unwrap();

        assert_eq!(encoded.len(), WORKING_COPY_RECORD_V1_SIZE);
        assert_eq!(encoded[VERSION_OFFSET], WORKING_COPY_RECORD_VERSION);
        assert_eq!(&encoded[ID_OFFSET..LOCATION_OFFSET], &[1; 16]);
        assert_eq!(&encoded[LOCATION_OFFSET..DESIRED_VIEW_OFFSET], &[2; 32]);
        assert_eq!(
            &encoded[DESIRED_VIEW_OFFSET..DESIRED_STATE_OFFSET],
            &record.desired_view.to_le_bytes()
        );
        assert_eq!(
            &encoded[DESIRED_STATE_OFFSET..MATERIALIZED_TAG_OFFSET],
            &[3; 32]
        );
        assert_eq!(encoded[MATERIALIZED_TAG_OFFSET], 1);
        assert_eq!(
            &encoded[MATERIALIZED_STATE_OFFSET..MANIFEST_TAG_OFFSET],
            &[4; 32]
        );
        assert_eq!(encoded[MANIFEST_TAG_OFFSET], 1);
        assert_eq!(&encoded[MANIFEST_OFFSET..], &[5; 32]);
        assert_eq!(decode_working_copy_record(&encoded).unwrap(), record);
    }

    #[test]
    fn working_copy_record_v1_roundtrips_absent_materialized_fields() {
        let mut record = record();
        record.materialized_state = None;
        record.materialized_manifest = None;

        let encoded = encode_working_copy_record(&record).unwrap();
        assert_eq!(encoded[MATERIALIZED_TAG_OFFSET], 0);
        assert!(encoded[MATERIALIZED_STATE_OFFSET..MANIFEST_TAG_OFFSET]
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(encoded[MANIFEST_TAG_OFFSET], 0);
        assert!(encoded[MANIFEST_OFFSET..].iter().all(|byte| *byte == 0));
        assert_eq!(decode_working_copy_record(&encoded).unwrap(), record);
    }

    #[test]
    fn working_copy_record_rejects_unknown_truncated_and_trailing_versions() {
        let encoded = encode_working_copy_record(&record()).unwrap();

        let mut unknown = encoded.clone();
        unknown[VERSION_OFFSET] = WORKING_COPY_RECORD_VERSION + 1;
        assert!(decode_working_copy_record(&unknown).is_err());
        assert!(decode_working_copy_record(&encoded[..encoded.len() - 1]).is_err());

        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_working_copy_record(&trailing).is_err());
    }

    #[test]
    fn working_copy_record_rejects_noncanonical_option_encoding() {
        let mut record = record();
        record.materialized_state = None;
        record.materialized_manifest = None;
        let mut encoded = encode_working_copy_record(&record).unwrap();

        encoded[MATERIALIZED_STATE_OFFSET] = 1;
        assert!(decode_working_copy_record(&encoded).is_err());

        let mut encoded = encode_working_copy_record(&record).unwrap();
        encoded[MATERIALIZED_TAG_OFFSET] = 2;
        assert!(decode_working_copy_record(&encoded).is_err());
    }

    #[test]
    fn working_copy_record_requires_state_for_manifest() {
        let mut record = record();
        record.materialized_state = None;
        assert!(encode_working_copy_record(&record).is_err());

        let mut encoded = encode_working_copy_record(&WorkingCopyRecord {
            materialized_state: None,
            materialized_manifest: None,
            ..record
        })
        .unwrap();
        encoded[MANIFEST_TAG_OFFSET] = 1;
        encoded[MANIFEST_OFFSET] = 1;
        assert!(decode_working_copy_record(&encoded).is_err());
    }

    #[test]
    fn working_copy_record_requires_nonzero_desired_view() {
        let mut record = record();
        record.desired_view = 0;
        assert!(encode_working_copy_record(&record).is_err());

        let mut encoded = encode_working_copy_record(&WorkingCopyRecord {
            desired_view: 1,
            ..record
        })
        .unwrap();
        encoded[DESIRED_VIEW_OFFSET..DESIRED_STATE_OFFSET].fill(0);
        assert!(decode_working_copy_record(&encoded).is_err());
    }
}

//! Immutable, content-addressed span operations in a change.
//!
//! A span refers to authored bytes by change hash and byte range. An edit
//! addresses a subrange of that span; it never rewrites the original bytes.

use crate::types::Hash;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"ATSP";
const VERSION: u8 = 2;

/// Maps an authored repository-local semantic ID to its portable change hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticOwner {
    /// Non-root NodeId used in this change's semantic operations.
    pub authored_id: u64,
    /// Content identity of the change that created that semantic ID.
    pub owner: Hash,
}

/// Portable identity of a file created by a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalFile {
    /// Hash of the creating change.
    pub owner: Hash,
    /// Ordinal of the file within that change.
    pub ordinal: u32,
}

/// Immutable authored byte range within a change's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSpan {
    /// Hash of the change that owns the bytes.
    pub owner: Hash,
    /// Inclusive byte offset in the owner's contents.
    pub start: u64,
    /// Exclusive byte offset in the owner's contents.
    pub end: u64,
}

/// A file root or an edit relative to an existing authored span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanOp {
    /// Introduce the initial authored range for a file.
    Root {
        /// Initial path; namespace ownership is represented separately.
        path: String,
        /// Ordinal used to form the portable file identity.
        ordinal: u32,
        /// Inclusive offset in this change's contents.
        start: u64,
        /// Exclusive offset in this change's contents.
        end: u64,
    },
    /// Replace a range of a previously authored span.
    Edit {
        /// Identity of the affected file.
        file: ExternalFile,
        /// Immutable span addressed by this edit.
        target: ExternalSpan,
        /// Inclusive offset relative to the start of the target span.
        start: u64,
        /// Exclusive offset relative to the start of the target span.
        end: u64,
        /// Inclusive offset of inserted bytes in this change's contents.
        insert_start: u64,
        /// Exclusive offset of inserted bytes in this change's contents.
        insert_end: u64,
    },
}

/// Versioned span operations. The prior metadata field preserves the existing
/// prototype's wire format; ordinary change metadata remains separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanPatch {
    /// Encoding version.
    pub version: u8,
    /// Metadata carried by older span payloads.
    pub prior_metadata: Vec<u8>,
    /// Operations in this change.
    pub ops: Vec<SpanOp>,
    /// Canonical mapping for non-placeholder semantic operation IDs.
    pub semantic_owners: Vec<SemanticOwner>,
}

// Preserve version-one payload bytes and hashes exactly.
#[derive(Serialize, Deserialize)]
struct SpanPatchV1 {
    version: u8,
    prior_metadata: Vec<u8>,
    ops: Vec<SpanOp>,
}

/// Invalid span encoding or local byte range.
#[derive(Debug, thiserror::Error)]
pub enum SpanPatchError {
    /// The payload is not marked as a span patch.
    #[error("not span patch metadata")]
    WrongMagic,
    /// The payload version is unsupported.
    #[error("unsupported span patch version {0}")]
    UnsupportedVersion(u8),
    /// Postcard data is malformed or has trailing bytes.
    #[error("invalid span patch encoding")]
    InvalidEncoding,
    /// An operation has an invalid local range.
    #[error("invalid span operation {index}: {reason}")]
    InvalidOperation {
        /// Index of the invalid operation.
        index: usize,
        /// Explanation of the invalid range.
        reason: &'static str,
    },
}

impl SpanPatch {
    /// Create a span patch with portable semantic-owner support.
    pub fn new(prior_metadata: Vec<u8>, ops: Vec<SpanOp>) -> Self {
        Self {
            version: VERSION,
            prior_metadata,
            ops,
            semantic_owners: Vec::new(),
        }
    }

    /// Encode the canonical span payload.
    pub fn encode(&self) -> Result<Vec<u8>, SpanPatchError> {
        if self.version != 1 && self.version != VERSION {
            return Err(SpanPatchError::UnsupportedVersion(self.version));
        }
        let mut bytes = MAGIC.to_vec();
        if self.version == 1 {
            if !self.semantic_owners.is_empty() {
                return Err(SpanPatchError::InvalidEncoding);
            }
            let legacy = SpanPatchV1 {
                version: 1,
                prior_metadata: self.prior_metadata.clone(),
                ops: self.ops.clone(),
            };
            bytes.extend(
                postcard::to_allocvec(&legacy).map_err(|_| SpanPatchError::InvalidEncoding)?,
            );
        } else {
            bytes.extend(postcard::to_allocvec(self).map_err(|_| SpanPatchError::InvalidEncoding)?);
        }
        Ok(bytes)
    }

    /// Decode one complete supported span payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, SpanPatchError> {
        let payload = bytes
            .strip_prefix(MAGIC)
            .ok_or(SpanPatchError::WrongMagic)?;
        let version = *payload.first().ok_or(SpanPatchError::InvalidEncoding)?;
        if version != 1 && version != VERSION {
            return Err(SpanPatchError::UnsupportedVersion(version));
        }
        let (patch, remaining) = if version == 1 {
            let (legacy, remaining): (SpanPatchV1, _) =
                postcard::take_from_bytes(payload).map_err(|_| SpanPatchError::InvalidEncoding)?;
            (
                Self {
                    version: legacy.version,
                    prior_metadata: legacy.prior_metadata,
                    ops: legacy.ops,
                    semantic_owners: Vec::new(),
                },
                remaining,
            )
        } else {
            postcard::take_from_bytes(payload).map_err(|_| SpanPatchError::InvalidEncoding)?
        };
        if !remaining.is_empty() {
            return Err(SpanPatchError::InvalidEncoding);
        }
        if patch.version != 1 && patch.version != VERSION {
            return Err(SpanPatchError::UnsupportedVersion(patch.version));
        }
        Ok(patch)
    }

    /// Check local byte bounds. Dependency closure, file ownership, and
    /// conflicts must additionally be checked against repository state.
    pub fn validate_content_ranges(&self, content_len: u64) -> Result<(), SpanPatchError> {
        if self.version != 1 && self.version != VERSION {
            return Err(SpanPatchError::UnsupportedVersion(self.version));
        }
        if self.version == 1 && !self.semantic_owners.is_empty() {
            return Err(SpanPatchError::InvalidEncoding);
        }
        if self
            .semantic_owners
            .iter()
            .any(|entry| entry.authored_id == 0 || entry.owner == Hash::NONE)
            || self
                .semantic_owners
                .windows(2)
                .any(|pair| pair[0].authored_id >= pair[1].authored_id)
        {
            return Err(SpanPatchError::InvalidEncoding);
        }
        let mut owner_hashes = std::collections::HashSet::new();
        if self
            .semantic_owners
            .iter()
            .any(|entry| !owner_hashes.insert(entry.owner))
        {
            return Err(SpanPatchError::InvalidEncoding);
        }
        for (index, op) in self.ops.iter().enumerate() {
            let reason = match op {
                SpanOp::Root { start, end, .. } if start > end || *end > content_len => {
                    Some("root range is reversed or outside the contents")
                }
                SpanOp::Edit { file, target, .. }
                    if file.owner == Hash::NONE || target.owner == Hash::NONE =>
                {
                    Some("file and target owners must be real changes")
                }
                SpanOp::Edit { target, .. } if target.start > target.end => {
                    Some("target span is reversed")
                }
                SpanOp::Edit {
                    target, start, end, ..
                } if start > end || *end > target.end.saturating_sub(target.start) => {
                    Some("edit range is reversed or outside the target span")
                }
                SpanOp::Edit {
                    insert_start,
                    insert_end,
                    ..
                } if insert_start > insert_end || *insert_end > content_len => {
                    Some("inserted range is reversed or outside the contents")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                return Err(SpanPatchError::InvalidOperation { index, reason });
            }
        }
        Ok(())
    }
}

/// Resolve a span change's authored semantic IDs under a receiving transaction.
/// The hashed change is never modified. Legacy V1 operations with external
/// IDs cannot be localized because their owner hashes were not recorded.
pub fn localize_file_ops<T: crate::pristine::GraphTxnT>(
    txn: &T,
    change: &crate::change::Change,
) -> anyhow::Result<Vec<crate::change::FileOps>> {
    use crate::pristine::{EffectiveProjectionClosure, ViewMembershipSet};
    use crate::types::NodeId;
    use anyhow::{ensure, Context};
    let Some(patch) = &change.hashed.span else {
        return Ok(change.file_ops().to_vec());
    };
    patch.validate_content_ranges(change.contents.len().try_into()?)?;
    let ids: std::collections::BTreeSet<_> = change
        .file_ops()
        .iter()
        .flat_map(|ops| ops.referenced_node_ids())
        .collect();
    let owners: std::collections::BTreeSet<_> = patch
        .semantic_owners
        .iter()
        .map(|entry| NodeId::new(entry.authored_id))
        .collect();
    ensure!(
        ids == owners && patch.semantic_owners.len() == owners.len(),
        "span semantic identities lack a complete portable owner mapping"
    );
    let deps = change
        .dependencies()
        .iter()
        .map(|hash| {
            txn.get_internal(hash)?
                .context("semantic dependency missing")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let closure = EffectiveProjectionClosure::try_from_membership(
        txn,
        &ViewMembershipSet::from_ordered(deps),
    )?;
    let mut mapping = std::collections::HashMap::new();
    for entry in &patch.semantic_owners {
        let id = txn
            .get_internal(&entry.owner)?
            .context("semantic owner missing")?;
        ensure!(
            closure.contains(id) && txn.get_external(id)? == Some(entry.owner),
            "span semantic owner is outside its dependency closure"
        );
        mapping.insert(NodeId::new(entry.authored_id), id);
    }
    let mut ops = change.file_ops().to_vec();
    for file in &mut ops {
        file.remap_referenced_node_ids(|id| {
            mapping
                .get(&id)
                .copied()
                .context("span semantic owner mapping is incomplete")
        })?;
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::format_v3::{ChangeReader, FORMAT_VERSION, SPAN_FORMAT_VERSION};
    use crate::change::{Change, ChangeHeader};
    use std::io::Cursor;

    #[test]
    fn legacy_span_payload_roundtrips_without_changing_its_bytes() {
        let legacy = SpanPatchV1 {
            version: 1,
            prior_metadata: b"legacy".to_vec(),
            ops: vec![SpanOp::Root {
                path: "file".into(),
                ordinal: 0,
                start: 0,
                end: 4,
            }],
        };
        let mut bytes = MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&legacy).unwrap());
        let decoded = SpanPatch::decode(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
        assert!(decoded.semantic_owners.is_empty());
        assert_eq!(decoded.encode().unwrap(), bytes);
        let mut change = Change::new(
            ChangeHeader::new("legacy span"),
            vec![],
            b"abc\n".to_vec(),
            vec![],
        );
        change.hashed.span = Some(decoded);
        let mut object = Vec::new();
        let original = change.serialize(&mut object).unwrap();
        let (decoded, hash) = Change::deserialize(&mut Cursor::new(&object)).unwrap();
        assert_eq!(original, hash);
        let mut reserialized = Vec::new();
        assert_eq!(decoded.serialize(&mut reserialized).unwrap(), original);
    }

    #[test]
    fn typed_span_is_distinct_from_opaque_metadata() {
        let patch = SpanPatch::new(
            Vec::new(),
            vec![SpanOp::Root {
                path: "file".into(),
                ordinal: 0,
                start: 0,
                end: 4,
            }],
        );
        let mut ordinary = Change::new(
            ChangeHeader::new("ordinary"),
            vec![],
            b"abc\n".to_vec(),
            vec![],
        );
        ordinary.hashed.metadata = patch.encode().unwrap();
        let mut ordinary_bytes = Vec::new();
        let ordinary_hash = ordinary.serialize(&mut ordinary_bytes).unwrap();
        assert_eq!(
            u32::from_le_bytes(ordinary_bytes[4..8].try_into().unwrap()),
            FORMAT_VERSION
        );
        let (decoded, hash) = Change::deserialize(&mut Cursor::new(&ordinary_bytes)).unwrap();
        assert_eq!(hash, ordinary_hash);
        assert_eq!(decoded.hashed.metadata, ordinary.hashed.metadata);
        assert!(decoded.hashed.span.is_none());

        let mut span = ordinary.clone();
        span.hashed.span = Some(patch.clone());
        let mut span_bytes = Vec::new();
        let span_hash = span.serialize(&mut span_bytes).unwrap();
        assert_ne!(span_hash, ordinary_hash);
        assert_eq!(
            u32::from_le_bytes(span_bytes[4..8].try_into().unwrap()),
            SPAN_FORMAT_VERSION
        );
        let (decoded, hash) = Change::deserialize(&mut Cursor::new(&span_bytes)).unwrap();
        assert_eq!(hash, span_hash);
        assert_eq!(decoded.hashed.metadata, ordinary.hashed.metadata);
        assert_eq!(decoded.hashed.span, Some(patch));
        assert!(!decoded.hashed.is_empty());
        let mut cursor = Cursor::new(&span_bytes);
        assert!(ChangeReader::open(&mut cursor)
            .unwrap()
            .graph_sections()
            .is_err());
        // The file header is unhashed. Downgrading it must never reinterpret a
        // span object as an ordinary change with the old identity.
        span_bytes[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        assert!(Change::deserialize(&mut Cursor::new(&span_bytes)).is_err());
    }

    #[test]
    fn span_and_semantic_operations_share_one_change() {
        use crate::change::FileOps;
        use crate::crdt::TrunkId;
        use crate::types::NodeId;
        let mut change = Change::with_file_ops(
            ChangeHeader::new("span and semantic"),
            vec![],
            vec![FileOps::edit(TrunkId::new(NodeId::ROOT, 0), "file".into())],
            b"abc\n".to_vec(),
            vec![],
        );
        change.hashed.span = Some(SpanPatch::new(
            vec![],
            vec![SpanOp::Root {
                path: "file".into(),
                ordinal: 0,
                start: 0,
                end: 4,
            }],
        ));
        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).unwrap();
        let (decoded, actual) = Change::deserialize(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(actual, hash);
        assert_eq!(decoded.hashed.span, change.hashed.span);
        assert_eq!(decoded.file_ops(), change.file_ops());
    }

    #[test]
    fn schema_three_requires_one_canonical_span_section() {
        use crate::change::format_v3::{ChangeWriter, FileHeader, HashDedupTable, WriterOptions};
        let mut header = FileHeader::builder().hash_table_entries(1).build();
        header.version = SPAN_FORMAT_VERSION;
        let hashes = HashDedupTable::new([0; 32]);
        let mut missing = Vec::new();
        let mut writer = ChangeWriter::new(&mut missing, WriterOptions::default());
        writer.write_file_header(&header).unwrap();
        writer.write_hash_table(&hashes).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("missing"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        assert!(writer.finalize().is_err());

        let mut malformed = SpanPatch::new(Vec::new(), Vec::new()).encode().unwrap();
        malformed.push(0);
        let mut bytes = Vec::new();
        let mut writer = ChangeWriter::new(&mut bytes, WriterOptions::default());
        writer.write_file_header(&header).unwrap();
        writer.write_hash_table(&hashes).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("malformed"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_span_section(&malformed).unwrap();
        assert!(writer.write_span_section(&malformed).is_err());
        writer.finalize().unwrap();
        assert!(Change::deserialize(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn ranges_and_owners_are_checked() {
        let owner = Hash::of(b"owner");
        let edit = SpanOp::Edit {
            file: ExternalFile { owner, ordinal: 0 },
            target: ExternalSpan {
                owner,
                start: 10,
                end: 20,
            },
            start: 1,
            end: 9,
            insert_start: 0,
            insert_end: 2,
        };
        assert!(SpanPatch::new(vec![], vec![edit.clone()])
            .validate_content_ranges(2)
            .is_ok());
        let mut bad = edit.clone();
        if let SpanOp::Edit { end, .. } = &mut bad {
            *end = 11;
        }
        assert!(SpanPatch::new(vec![], vec![bad])
            .validate_content_ranges(2)
            .is_err());
        let mut bad = edit;
        if let SpanOp::Edit { file, .. } = &mut bad {
            file.owner = Hash::NONE;
        }
        assert!(SpanPatch::new(vec![], vec![bad])
            .validate_content_ranges(2)
            .is_err());
    }

    #[test]
    fn opaque_schema_two_matches_existing_wire_bytes() {
        use crate::types::Base32;
        let hex: String = include_str!("format_v3/fixtures/span_opaque_v2_reference.hex")
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let expected: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mut header = ChangeHeader::new("opaque v2 fixture");
        header.timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut change = Change::new(header, vec![], b"old bytes\n".to_vec(), vec![]);
        change.hashed.metadata = b"ATSP\x01\x00\x00".to_vec();
        let mut actual = Vec::new();
        let hash = change.serialize(&mut actual).unwrap();
        assert_eq!(
            hash.to_base32(),
            "Z6ICEFZQ3TKMR4YIRONQAAA4GF6CKGQPMZJWVK6YU5O5SAX456HA"
        );
        assert_eq!(actual, expected);
        let (decoded, verified) = Change::deserialize(&mut expected.as_slice()).unwrap();
        assert_eq!(verified, hash);
        assert!(decoded.hashed.span.is_none());
        assert_eq!(decoded.hashed.metadata, change.hashed.metadata);
    }
}

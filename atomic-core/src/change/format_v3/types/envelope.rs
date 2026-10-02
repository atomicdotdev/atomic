//! Frozen schema-version-2 payload for the hashed HEADER section.

use super::HashIndex;
use crate::change::classification::validate_canonical_hashes;
use crate::change::{
    CausalFrontier, ChangeHeader, ChangeKind, ChangeOrigin, ChangeValidationError, GitDerivation,
    HashedChange,
};
use crate::{GitHashAlgorithm, GitObjectId, Hash, WorkingCopyId};
use serde::{Deserialize, Serialize};

use crate::change::format_v3::{FormatError, FormatResult, HashDedupTable};

/// Decoded domain values carried by a schema-version-2 HEADER payload.
pub(crate) struct DecodedChangeHeaderV2 {
    pub header: ChangeHeader,
    pub kind: ChangeKind,
    pub supersedes: Option<Hash>,
    pub origin: ChangeOrigin,
    pub causal_frontier: CausalFrontier,
    pub extra_known: Vec<Hash>,
    pub metadata: Vec<u8>,
}

/// Encode a complete schema-version-2 HEADER payload.
pub(crate) fn encode_change_header_v2(
    change: &HashedChange,
    table: &HashDedupTable,
) -> FormatResult<Vec<u8>> {
    let envelope = ChangeHeaderV2 {
        header: change.header.clone(),
        kind: WireChangeKindV2::from_domain(&change.kind),
        supersedes: change
            .supersedes
            .as_ref()
            .map(|hash| table.require(hash.as_bytes()))
            .transpose()?,
        origin: WireChangeOriginV2::from_domain(&change.origin)?,
        causal_frontier: encode_hashes(change.causal_frontier.roots(), table)?,
        extra_known: encode_hashes(&change.extra_known, table)?,
        metadata: change.metadata.clone(),
    };
    postcard::to_allocvec(&envelope).map_err(FormatError::from)
}

/// Encode the default Durable/Native envelope used by the low-level writer API.
pub(crate) fn encode_native_change_header_v2(header: &ChangeHeader) -> FormatResult<Vec<u8>> {
    let envelope = ChangeHeaderV2 {
        header: header.clone(),
        kind: WireChangeKindV2::Durable,
        supersedes: None,
        origin: WireChangeOriginV2::Native,
        causal_frontier: Vec::new(),
        extra_known: Vec::new(),
        metadata: Vec::new(),
    };
    postcard::to_allocvec(&envelope).map_err(FormatError::from)
}

/// Decode a complete schema-version-2 HEADER payload.
pub(crate) fn decode_change_header_v2(
    bytes: &[u8],
    table: &HashDedupTable,
) -> FormatResult<DecodedChangeHeaderV2> {
    let envelope: ChangeHeaderV2 = postcard::from_bytes(bytes)?;
    let kind = envelope.kind.into_domain();
    let supersedes = envelope
        .supersedes
        .map(|index| decode_hash(index, table, "supersedes"))
        .transpose()?;
    let origin = envelope.origin.into_domain()?;
    let causal_frontier = CausalFrontier::new(decode_hashes(
        &envelope.causal_frontier,
        table,
        "causal_frontier",
    )?)
    .map_err(invalid_change_metadata)?;
    let extra_known = decode_hashes(&envelope.extra_known, table, "extra_known")?;
    validate_canonical_hashes("extra_known", &extra_known).map_err(invalid_change_metadata)?;

    Ok(DecodedChangeHeaderV2 {
        header: envelope.header,
        kind,
        supersedes,
        origin,
        causal_frontier,
        extra_known,
        metadata: envelope.metadata,
    })
}

/// Decode only the human header for the low-level `ReadSection` convenience API.
pub(crate) fn decode_change_header_only_v2(bytes: &[u8]) -> Result<ChangeHeader, postcard::Error> {
    postcard::from_bytes::<ChangeHeaderV2>(bytes).map(|envelope| envelope.header)
}

pub(crate) fn invalid_change_metadata(error: ChangeValidationError) -> FormatError {
    FormatError::InvalidChangeMetadata {
        reason: error.to_string(),
    }
}

fn encode_hashes(hashes: &[Hash], table: &HashDedupTable) -> FormatResult<Vec<HashIndex>> {
    hashes
        .iter()
        .map(|hash| table.require(hash.as_bytes()))
        .collect()
}

pub(crate) fn decode_hash(
    index: HashIndex,
    table: &HashDedupTable,
    field: &'static str,
) -> FormatResult<Hash> {
    let bytes = table.resolve_required(index)?;
    let hash = Hash::from_bytes(*bytes);
    if hash == Hash::NONE {
        return Err(FormatError::InvalidChangeMetadata {
            reason: format!("{field} references the reserved zero/self hash at index {index}"),
        });
    }
    Ok(hash)
}

fn decode_hashes(
    indices: &[HashIndex],
    table: &HashDedupTable,
    field: &'static str,
) -> FormatResult<Vec<Hash>> {
    indices
        .iter()
        .map(|index| decode_hash(*index, table, field))
        .collect()
}

/// The field order and enum discriminants below are frozen for schema version 2.
#[derive(Serialize, Deserialize)]
struct ChangeHeaderV2 {
    header: ChangeHeader,
    kind: WireChangeKindV2,
    supersedes: Option<HashIndex>,
    origin: WireChangeOriginV2,
    causal_frontier: Vec<HashIndex>,
    extra_known: Vec<HashIndex>,
    metadata: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
enum WireChangeKindV2 {
    Durable,
    Snapshot { working_copy: [u8; 16] },
}

impl WireChangeKindV2 {
    fn from_domain(kind: &ChangeKind) -> Self {
        match kind {
            ChangeKind::Durable => Self::Durable,
            ChangeKind::Snapshot { working_copy } => Self::Snapshot {
                working_copy: *working_copy.as_bytes(),
            },
        }
    }

    fn into_domain(self) -> ChangeKind {
        match self {
            Self::Durable => ChangeKind::Durable,
            Self::Snapshot { working_copy } => ChangeKind::Snapshot {
                working_copy: WorkingCopyId::from_bytes(working_copy),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
enum WireChangeOriginV2 {
    Native,
    GitSynthesized {
        commit: WireGitObjectIdV2,
        parents: Vec<WireGitObjectIdV2>,
        derivation: WireGitDerivationV2,
    },
    GitResolution {
        merge_commit: WireGitObjectIdV2,
        parents: Vec<WireGitObjectIdV2>,
    },
}

impl WireChangeOriginV2 {
    fn from_domain(origin: &ChangeOrigin) -> FormatResult<Self> {
        match origin {
            ChangeOrigin::Native => Ok(Self::Native),
            ChangeOrigin::GitSynthesized {
                commit,
                parents,
                derivation,
            } => Ok(Self::GitSynthesized {
                commit: WireGitObjectIdV2::from_domain(commit)?,
                parents: parents
                    .iter()
                    .map(WireGitObjectIdV2::from_domain)
                    .collect::<FormatResult<_>>()?,
                derivation: WireGitDerivationV2::from_domain(*derivation),
            }),
            ChangeOrigin::GitResolution {
                merge_commit,
                parents,
            } => Ok(Self::GitResolution {
                merge_commit: WireGitObjectIdV2::from_domain(merge_commit)?,
                parents: parents
                    .iter()
                    .map(WireGitObjectIdV2::from_domain)
                    .collect::<FormatResult<_>>()?,
            }),
        }
    }

    fn into_domain(self) -> FormatResult<ChangeOrigin> {
        let origin = match self {
            Self::Native => ChangeOrigin::Native,
            Self::GitSynthesized {
                commit,
                parents,
                derivation,
            } => ChangeOrigin::GitSynthesized {
                commit: commit.into_domain()?,
                parents: parents
                    .into_iter()
                    .map(WireGitObjectIdV2::into_domain)
                    .collect::<FormatResult<_>>()?,
                derivation: derivation.into_domain(),
            },
            Self::GitResolution {
                merge_commit,
                parents,
            } => ChangeOrigin::GitResolution {
                merge_commit: merge_commit.into_domain()?,
                parents: parents
                    .into_iter()
                    .map(WireGitObjectIdV2::into_domain)
                    .collect::<FormatResult<_>>()?,
            },
        };
        origin.validate().map_err(invalid_change_metadata)?;
        Ok(origin)
    }
}

#[derive(Serialize, Deserialize)]
enum WireGitObjectIdV2 {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

impl WireGitObjectIdV2 {
    fn from_domain(oid: &GitObjectId) -> FormatResult<Self> {
        match oid.algorithm() {
            GitHashAlgorithm::Sha1 => {
                let bytes =
                    oid.as_bytes()
                        .try_into()
                        .map_err(|_| FormatError::InvalidChangeMetadata {
                            reason: "SHA-1 Git object ID does not contain 20 bytes".to_string(),
                        })?;
                Ok(Self::Sha1(bytes))
            }
            GitHashAlgorithm::Sha256 => {
                let bytes =
                    oid.as_bytes()
                        .try_into()
                        .map_err(|_| FormatError::InvalidChangeMetadata {
                            reason: "SHA-256 Git object ID does not contain 32 bytes".to_string(),
                        })?;
                Ok(Self::Sha256(bytes))
            }
        }
    }

    fn into_domain(self) -> FormatResult<GitObjectId> {
        let result = match self {
            Self::Sha1(bytes) => GitObjectId::new(GitHashAlgorithm::Sha1, bytes.to_vec()),
            Self::Sha256(bytes) => GitObjectId::new(GitHashAlgorithm::Sha256, bytes.to_vec()),
        };
        result.map_err(|error| FormatError::InvalidChangeMetadata {
            reason: error.to_string(),
        })
    }
}

#[derive(Serialize, Deserialize)]
enum WireGitDerivationV2 {
    Root,
    FirstParent,
    MultiParent,
    Squash,
    EmptyCommit,
    RewriteCandidate,
}

impl WireGitDerivationV2 {
    fn from_domain(derivation: GitDerivation) -> Self {
        match derivation {
            GitDerivation::Root => Self::Root,
            GitDerivation::FirstParent => Self::FirstParent,
            GitDerivation::MultiParent => Self::MultiParent,
            GitDerivation::Squash => Self::Squash,
            GitDerivation::EmptyCommit => Self::EmptyCommit,
            GitDerivation::RewriteCandidate => Self::RewriteCandidate,
        }
    }

    fn into_domain(self) -> GitDerivation {
        match self {
            Self::Root => GitDerivation::Root,
            Self::FirstParent => GitDerivation::FirstParent,
            Self::MultiParent => GitDerivation::MultiParent,
            Self::Squash => GitDerivation::Squash,
            Self::EmptyCommit => GitDerivation::EmptyCommit,
            Self::RewriteCandidate => GitDerivation::RewriteCandidate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hash_rejects_out_of_bounds_and_zero_self_slot() {
        let table = HashDedupTable::new([0; 32]);
        assert!(decode_hash(0, &table, "test").is_err());
        assert!(decode_hash(1, &table, "test").is_err());
        assert!(decode_hash(u16::MAX, &table, "test").is_err());
    }

    #[test]
    fn v2_header_rejects_out_of_bounds_atomic_hash_indices() {
        let envelope = ChangeHeaderV2 {
            header: ChangeHeader::new("malformed"),
            kind: WireChangeKindV2::Durable,
            supersedes: None,
            origin: WireChangeOriginV2::Native,
            causal_frontier: Vec::new(),
            extra_known: vec![7],
            metadata: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&envelope).unwrap();
        let table = HashDedupTable::new([0; 32]);

        assert!(matches!(
            decode_change_header_v2(&bytes, &table),
            Err(FormatError::HashIndexOutOfBounds { index: 7, .. })
        ));
    }

    #[test]
    fn v2_header_rejects_noncanonical_extra_known_hashes() {
        let envelope = ChangeHeaderV2 {
            header: ChangeHeader::new("noncanonical"),
            kind: WireChangeKindV2::Durable,
            supersedes: None,
            origin: WireChangeOriginV2::Native,
            causal_frontier: Vec::new(),
            extra_known: vec![1, 2],
            metadata: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&envelope).unwrap();
        let table = HashDedupTable::from_hashes(vec![[0; 32], [2; 32], [1; 32]]).unwrap();

        assert!(matches!(
            decode_change_header_v2(&bytes, &table),
            Err(FormatError::InvalidChangeMetadata { .. })
        ));
    }

    #[test]
    fn v2_header_rejects_mixed_git_algorithms_from_wire() {
        let envelope = ChangeHeaderV2 {
            header: ChangeHeader::new("mixed algorithms"),
            kind: WireChangeKindV2::Durable,
            supersedes: None,
            origin: WireChangeOriginV2::GitSynthesized {
                commit: WireGitObjectIdV2::Sha1([1; 20]),
                parents: vec![WireGitObjectIdV2::Sha256([2; 32])],
                derivation: WireGitDerivationV2::FirstParent,
            },
            causal_frontier: Vec::new(),
            extra_known: Vec::new(),
            metadata: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&envelope).unwrap();
        let table = HashDedupTable::new([0; 32]);

        assert!(matches!(
            decode_change_header_v2(&bytes, &table),
            Err(FormatError::InvalidChangeMetadata { .. })
        ));
    }

    #[test]
    fn tagged_git_object_ids_roundtrip_both_algorithms() {
        for oid in [
            GitObjectId::new(GitHashAlgorithm::Sha1, vec![1; 20]).unwrap(),
            GitObjectId::new(GitHashAlgorithm::Sha256, vec![2; 32]).unwrap(),
        ] {
            let decoded = WireGitObjectIdV2::from_domain(&oid)
                .unwrap()
                .into_domain()
                .unwrap();
            assert_eq!(decoded, oid);
        }
    }
}

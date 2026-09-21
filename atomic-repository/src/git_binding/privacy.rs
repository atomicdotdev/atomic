//! Git-safe binding serialization: what may enter a Git binding tree or pack.
//!
//! The boundary is an allowlist, not a filter (RFC §5.6, §8.6, §12.13): a
//! binding tree contains exactly the canonical signed binding bytes, the
//! signed attestation summary, and — only via
//! [`binding_pack_records`] — V3 change files that hash-verify. Provenance,
//! attestation, and view sidecar families from the Atomic transport
//! ([`atomic_objects::sync::ObjectFamily`]) are refused here even if a caller
//! asks for them, because they can carry unhashed private payloads. Private
//! evidence travels only through the Atomic remote boundary.
//!
//! The privacy guarantee is tested adversarially: distinctive sentinels are
//! seeded into provenance/attestation content and the *packed* Git objects
//! are grepped for them (see `atomic-cli/tests/git_binding_cb6a_test.rs`).

use atomic_core::Hash;
use atomic_objects::{ObjectFamily, ObjectRecord};

/// Blob name of the canonical signed binding inside a binding tree.
pub const BINDING_BLOB_NAME: &str = "binding.cbor";

/// Blob name of the signed attestation summary inside a binding tree.
pub const ATTESTATION_SUMMARY_BLOB_NAME: &str = "attestation-summary.cbor";

/// Blob name of the optional bounded change pack inside a binding tree.
///
/// The pack assembly itself is CB-6B (bounded transport); this unit fixes the
/// name and the audited family boundary so the layout cannot drift.
#[allow(dead_code)]
pub const CHANGES_PACK_BLOB_NAME: &str = "changes.pack";

/// Blob name of the optional complete conflict-object pack inside a binding
/// tree (RFC §8.3, CB-8B).
///
/// Exact restoration of a conflict snapshot commit requires the complete
/// Atomic conflict objects; markers plus a hash are insufficient. The pack
/// carries `ConflictSetObject` canonical bytes only — graph facts, never
/// provenance or attestation payloads.
pub const CONFLICTS_PACK_BLOB_NAME: &str = "conflicts.pack";

/// Errors from the Git-safe serialization boundary.
#[derive(Debug, thiserror::Error)]
pub enum BindingTreeError {
    #[error("cannot encode binding bytes: {0}")]
    Encode(String),
    #[error("binding pack object key {key} does not match blake3 of its bytes")]
    HashMismatch { key: String },
}

/// The exact Git-safe blobs of one binding publication, in tree order.
///
/// Every byte here comes from an allowlisted, separately signed or
/// hash-bound field. Nothing else may be added to a binding tree.
pub fn binding_tree_entries(
    binding: &super::codec::GitStateBinding,
    summary: Option<&super::summary::SignedAttestationSummary>,
) -> Result<Vec<(&'static str, Vec<u8>)>, BindingTreeError> {
    let mut entries = vec![(
        BINDING_BLOB_NAME,
        binding.encode(),
    )];
    if let Some(summary) = summary {
        entries.push((ATTESTATION_SUMMARY_BLOB_NAME, summary.encode()));
    }
    Ok(entries)
}

/// Select the change objects allowed into the optional `changes.pack`.
///
/// This is the audited sync-pack boundary (RFC §8.6): only
/// [`ObjectFamily::Change`] records may enter a binding pack, and each must
/// hash-verify (`blake3(bytes) == key`) so the pack stays content-addressed.
/// Provenance/attestation/view sidecars — the families that can contain
/// unhashed private payloads — are refused, never silently copied into Git.
pub fn binding_pack_records(
    candidates: &[ObjectRecord],
) -> Result<Vec<ObjectRecord>, BindingTreeError> {
    let mut allowed = Vec::with_capacity(candidates.len());
    for record in candidates {
        match record.family {
            ObjectFamily::Change => {}
            family => {
                return Err(BindingTreeError::Encode(format!(
                    "binding pack refused {family:?} object {key}: only V3 change files may enter Git",
                    key = record.key
                )))
            }
        }
        if Hash::of(&record.bytes).to_hex() != record.key {
            return Err(BindingTreeError::HashMismatch {
                key: record.key.clone(),
            });
        }
        allowed.push(record.clone());
    }
    Ok(allowed)
}

/// Structurally validate conflict-pack bytes without a claimed identity.
///
/// Publication refuses a pack that cannot decode or fails its structural
/// self-check; the identity check against the projection commit's
/// `atomic-conflict <hash>` header happens wherever both the header and the
/// pack are known (projection, fetch, restore).
pub fn conflicts_pack_self_validates(
    bytes: &[u8],
) -> Result<atomic_core::Hash, ConflictPackError> {
    use crate::repository::ConflictSetObject;
    let object = ConflictSetObject::decode(bytes)
        .map_err(|error| ConflictPackError::Decode(error.to_string()))?;
    object
        .validate()
        .map_err(|error| ConflictPackError::Invalid(error.to_string()))?;
    object
        .hash()
        .map_err(|error| ConflictPackError::Invalid(error.to_string()))
}

/// Verify conflict-pack bytes against the claimed conflict-set identity.
///
/// The pack is the canonical [`crate::repository::ConflictSetObject`] bytes
/// (not a family bag): its identity is re-derived here so a forged or stale
/// pack cannot ride alongside a binding commit. Conflict objects carry graph
/// facts only — identities, base/sides, modes, claimants — so they may enter
/// Git; provenance/attestation payloads still never do.
pub fn validate_conflicts_pack(
    bytes: &[u8],
    claimed_hash: &atomic_core::Hash,
) -> Result<(), ConflictPackError> {
    use atomic_core::types::Base32;
    let actual = conflicts_pack_self_validates(bytes)?;
    if actual != *claimed_hash {
        return Err(ConflictPackError::HashMismatch {
            claimed: claimed_hash.to_base32(),
            actual: actual.to_base32(),
        });
    }
    Ok(())
}

/// Errors from conflict-pack admission into a binding tree.
#[derive(Debug, thiserror::Error)]
pub enum ConflictPackError {
    #[error("conflicts.pack decode failed: {0}")]
    Decode(String),
    #[error("conflicts.pack hash mismatch: claimed {claimed}, actual {actual}")]
    HashMismatch { claimed: String, actual: String },
    #[error("conflicts.pack is structurally invalid: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(family: ObjectFamily, bytes: &[u8]) -> ObjectRecord {
        ObjectRecord::new(family, Hash::of(bytes).to_hex(), bytes.to_vec())
    }

    #[test]
    fn pack_refuses_private_sidecar_families() {
        let change = record(ObjectFamily::Change, b"v3 change file bytes");
        let provenance = record(ObjectFamily::Provenance, b"provenance sidecar");
        let attest = record(ObjectFamily::Attest, b"attest sidecar");
        let view = record(ObjectFamily::View, b"view snapshot");

        let only_change = binding_pack_records(std::slice::from_ref(&change)).expect("change only");
        assert_eq!(only_change.len(), 1);

        for family in [provenance, attest, view] {
            let refused = binding_pack_records(std::slice::from_ref(&family));
            assert!(refused.is_err(), "{:?} must not enter a binding pack", family.family);
        }
    }

    #[test]
    fn pack_verifies_content_addressing() {
        let bytes = b"v3 change bytes";
        let mut forged = record(ObjectFamily::Change, bytes);
        forged.key = Hash::of(b"different bytes").to_hex();
        let refused = binding_pack_records(std::slice::from_ref(&forged));
        assert!(
            refused.is_err(),
            "a pack object whose key does not match its bytes must be refused"
        );
    }

    #[test]
    fn tree_entries_are_exactly_the_public_blobs() {
        // Constructed through the repository-level helper in binding_store
        // tests; here we only pin the public names so they cannot drift
        // silently from the RFC §8.6 layout.
        assert_eq!(BINDING_BLOB_NAME, "binding.cbor");
        assert_eq!(ATTESTATION_SUMMARY_BLOB_NAME, "attestation-summary.cbor");
        assert_eq!(CHANGES_PACK_BLOB_NAME, "changes.pack");
    }
}

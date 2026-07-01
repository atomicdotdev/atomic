//! Data Integrity proofs over canonical nodes (`eddsa-jcs-2022`).
//!
//! Signing covers the JCS-canonical node minus its `proof` (the standard Data
//! Integrity shape). We reuse atomic's existing Ed25519 `Signer`/`PublicKey`
//! and sign over the same canonical bytes the content hash uses, via the one
//! `jcs` entry point — so hash and signature can never disagree.
//!
//! `proofValue` is a multibase string. We use base32-upper (`B`) via
//! `data-encoding` (already in-tree) rather than base58btc (`z`) to avoid an
//! extra dependency; it is a valid multibase encoding and the prefix is
//! swappable when a base58 dependency is admitted.

use atomic_identity::identity::Identity;
use atomic_identity::keypair::{KeyPair, PublicKey};
use atomic_identity::signing::{Signature, Signer};
use serde_json::Value;

use crate::did;
use crate::error::{CanonicalError, Result};
use crate::hash;
use crate::jcs;
use crate::memory::MemoryNode;
use crate::node::{CanonicalNode, Proof};

pub const CRYPTOSUITE: &str = "eddsa-jcs-2022";
pub const PROOF_TYPE: &str = "DataIntegrityProof";
pub const PROOF_PURPOSE: &str = "assertionMethod";
/// Multibase prefix for base32-upper, no padding.
const MULTIBASE_BASE32_UPPER: char = 'B';

/// Canonical property names. Hardcoded because the closed vocabulary
/// standardizes them across *all* node types (Intent, Memory, and any future
/// PROV `@graph`), so the "which keys are excluded from signing/hashing" rule
/// has exactly one owner — these consts and the two views below.
pub const PROP_PROOF: &str = "proof";
pub const PROP_CONTENT_HASH: &str = "contentHash";
pub const PROP_ATTRIBUTED_TO: &str = "attributedTo";

/// The value covered by a signature: the document minus its `proof` (the
/// standard Data Integrity shape). `contentHash` is retained so the signature
/// also commits to the hash. This is the single site that owns the exclusion.
pub fn signing_view(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(obj) = value.as_object_mut() {
        obj.remove(PROP_PROOF);
    }
    value
}

/// The value covered by the content hash: excludes both `proof` and
/// `contentHash` (you cannot hash the hash).
pub fn hashing_view(value: &Value) -> Value {
    let mut value = signing_view(value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove(PROP_CONTENT_HASH);
    }
    value
}

/// Generic attest over a JSON-LD value — the *one* canonicalization/signing
/// path all typed nodes share. Fills `attributedTo` (from the identity's
/// `did:atomic`) when absent, computes the content hash over `hashing_view`,
/// signs `jcs(signing_view)`, and attaches the proof. Returns the value.
pub fn attest_value(mut value: Value, identity: &Identity, keypair: &KeyPair) -> Value {
    let did = did::did_for_public_key(&identity.public_key);

    if let Some(obj) = value.as_object_mut() {
        // Fill attributedTo only if there is no non-empty value already.
        let has_author = obj
            .get(PROP_ATTRIBUTED_TO)
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_author {
            obj.insert(PROP_ATTRIBUTED_TO.to_string(), Value::String(did.clone()));
        }
    }

    // Hash first (over the value without proof/contentHash), then sign (over
    // the value with contentHash, without proof).
    let content_hash = hash::content_hash(&hashing_view(&value));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(PROP_CONTENT_HASH.to_string(), Value::String(content_hash));
    }

    let signing_bytes = jcs::canonicalize(&signing_view(&value)).into_bytes();
    let signature = Signer::new(keypair).sign(&signing_bytes);
    let proof = Proof {
        type_: PROOF_TYPE.to_string(),
        cryptosuite: CRYPTOSUITE.to_string(),
        verification_method: did::verification_method(&did),
        proof_purpose: PROOF_PURPOSE.to_string(),
        proof_value: encode_proof_value(&signature),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            PROP_PROOF.to_string(),
            serde_json::to_value(proof).expect("proof serialization is infallible"),
        );
    }
    value
}

/// Generic verify over a JSON-LD value — the same three checks as the typed
/// path: (1) the content hash recomputes over `hashing_view`, (2) the signature
/// verifies over `jcs(signing_view)`, (3) the proof's verificationMethod DID
/// belongs to this key.
pub fn verify_value(value: &Value, public_key: &PublicKey) -> Result<()> {
    let obj = value
        .as_object()
        .ok_or_else(|| CanonicalError::Proof("value is not a JSON object".into()))?;

    // 1. Content hash integrity.
    let expected = obj
        .get(PROP_CONTENT_HASH)
        .and_then(Value::as_str)
        .ok_or_else(|| CanonicalError::Proof("node carries no contentHash".into()))?;
    let actual = hash::content_hash(&hashing_view(value));
    if expected != actual {
        return Err(CanonicalError::HashMismatch {
            expected: expected.to_string(),
            actual,
        });
    }

    // 2. Signature over the canonical signing bytes.
    let proof: Proof = obj
        .get(PROP_PROOF)
        .cloned()
        .and_then(|p| serde_json::from_value(p).ok())
        .ok_or_else(|| CanonicalError::Proof("node carries no proof".into()))?;

    // The proof's own metadata is stripped by `signing_view`, so it is not
    // covered by the signature. Enforce the suite/purpose/type we expect here,
    // otherwise a stored node's declared cryptosuite is silently mutable.
    if proof.type_ != PROOF_TYPE
        || proof.cryptosuite != CRYPTOSUITE
        || proof.proof_purpose != PROOF_PURPOSE
    {
        return Err(CanonicalError::Verification(format!(
            "unexpected proof metadata: type='{}' cryptosuite='{}' proofPurpose='{}'",
            proof.type_, proof.cryptosuite, proof.proof_purpose
        )));
    }

    let signature = decode_proof_value(&proof.proof_value)?;
    let signing_bytes = jcs::canonicalize(&signing_view(value)).into_bytes();
    signature
        .verify(&signing_bytes, public_key)
        .map_err(|e| CanonicalError::Verification(e.to_string()))?;

    // 3. The proof's verificationMethod DID must belong to this key.
    let did = did::did_from_verification_method(&proof.verification_method);
    if !did::did_matches_public_key(did, public_key) {
        return Err(CanonicalError::Verification(
            "verificationMethod DID does not match the public key".into(),
        ));
    }
    Ok(())
}

/// Produce a fully attested Intent node — a thin wrapper over
/// [`attest_value`] so hash and signature can never drift from any other node
/// type. The round trip through serde is lossless for an already-constructed
/// node (all fields are symmetric), so `from_value` cannot fail.
pub fn attest(node: CanonicalNode, identity: &Identity, keypair: &KeyPair) -> CanonicalNode {
    let value = attest_value(node.to_value(), identity, keypair);
    serde_json::from_value(value).expect("attested intent re-deserializes")
}

/// Verify an attested Intent node — thin wrapper over [`verify_value`].
pub fn verify(node: &CanonicalNode, public_key: &PublicKey) -> Result<()> {
    verify_value(&node.to_value(), public_key)
}

/// Produce a fully attested Memory node — the same thin wrapper over
/// [`attest_value`] the Intent path uses, so both node types sign through one
/// canonicalization path and their hashes/signatures cannot drift.
pub fn attest_memory(node: MemoryNode, identity: &Identity, keypair: &KeyPair) -> MemoryNode {
    let value = attest_value(node.to_value(), identity, keypair);
    serde_json::from_value(value).expect("attested memory re-deserializes")
}

/// Verify an attested Memory node — thin wrapper over [`verify_value`].
pub fn verify_memory(node: &MemoryNode, public_key: &PublicKey) -> Result<()> {
    verify_value(&node.to_value(), public_key)
}

fn encode_proof_value(signature: &Signature) -> String {
    let mut s = String::new();
    s.push(MULTIBASE_BASE32_UPPER);
    s.push_str(&data_encoding::BASE32_NOPAD.encode(signature.as_bytes()));
    s
}

fn decode_proof_value(value: &str) -> Result<Signature> {
    let mut chars = value.chars();
    match chars.next() {
        Some(MULTIBASE_BASE32_UPPER) => {}
        _ => {
            return Err(CanonicalError::Proof(format!(
                "unsupported multibase prefix in proofValue: {value:.4}…"
            )))
        }
    }
    let body = &value[MULTIBASE_BASE32_UPPER.len_utf8()..];
    let bytes = data_encoding::BASE32_NOPAD
        .decode(body.as_bytes())
        .map_err(|e| CanonicalError::Proof(format!("bad base32 proofValue: {e}")))?;
    Signature::from_slice(&bytes).map_err(|e| CanonicalError::Proof(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::CONTEXT_URL;
    use serde_json::json;

    fn dev_identity() -> (Identity, KeyPair) {
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        (id, kp)
    }

    fn minimal_value() -> Value {
        json!({
            "@context": CONTEXT_URL,
            "@type": "Intent",
            "@id": "urn:atomic:intent:generic-1",
            "title": "A generic value node",
            "status": "todo"
        })
    }

    #[test]
    fn attest_value_then_verify_value_roundtrips() {
        let (id, kp) = dev_identity();
        let attested = attest_value(minimal_value(), &id, &kp);

        // The generic path filled the excluded-later fields.
        let obj = attested.as_object().unwrap();
        assert!(obj.get(PROP_CONTENT_HASH).unwrap().as_str().unwrap().starts_with("blake3:"));
        assert!(obj.get(PROP_PROOF).is_some());
        assert!(obj.get(PROP_ATTRIBUTED_TO).unwrap().as_str().unwrap().starts_with("did:atomic:"));

        // Verifies with the right key, independent of CanonicalNode.
        verify_value(&attested, &kp.public).expect("generic verify should succeed");

        // Tamper a signed field → the stored contentHash no longer matches.
        let mut tampered = attested.clone();
        tampered.as_object_mut().unwrap().insert("status".into(), Value::String("done".into()));
        let err = verify_value(&tampered, &kp.public).unwrap_err();
        assert!(
            matches!(err, CanonicalError::HashMismatch { .. }),
            "expected HashMismatch, got {err:?}"
        );

        // Wrong key → verification error.
        let other = KeyPair::generate();
        assert!(verify_value(&attested, &other.public).is_err());
    }

    #[test]
    fn typed_wrapper_matches_value_core() {
        let (id, kp) = dev_identity();
        let node = CanonicalNode {
            context: CONTEXT_URL.to_string(),
            type_: "Intent".to_string(),
            id: "urn:atomic:intent:wrapper-1".to_string(),
            human_key: "W-1".to_string(),
            title: "Wrapper equivalence".to_string(),
            status: "todo".to_string(),
            priority: None,
            view: None,
            motivated_by: None,
            informed_by: Vec::new(),
            has_acceptance_criterion: Vec::new(),
            has_task: Vec::new(),
            has_scope_in: Vec::new(),
            has_scope_out: Vec::new(),
            has_constraint: Vec::new(),
            depends_on: Vec::new(),
            why: Some("a reason".to_string()),
            content_hash: None,
            attributed_to: None,
            created_at: "2026-06-25T00:00:00Z".to_string(),
            proof: None,
        };

        // The typed wrapper must add nothing over the value core: attesting the
        // node equals from_value(attest_value(node.to_value())).
        let via_wrapper = attest(node.clone(), &id, &kp);
        let via_core: CanonicalNode =
            serde_json::from_value(attest_value(node.to_value(), &id, &kp)).unwrap();

        assert_eq!(via_wrapper.content_hash, via_core.content_hash);
        assert_eq!(
            via_wrapper.proof.as_ref().map(|p| &p.proof_value),
            via_core.proof.as_ref().map(|p| &p.proof_value)
        );
    }
}

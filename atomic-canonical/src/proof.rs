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

use crate::did;
use crate::error::{CanonicalError, Result};
use crate::node::{CanonicalNode, Proof};

pub const CRYPTOSUITE: &str = "eddsa-jcs-2022";
pub const PROOF_TYPE: &str = "DataIntegrityProof";
pub const PROOF_PURPOSE: &str = "assertionMethod";
/// Multibase prefix for base32-upper, no padding.
const MULTIBASE_BASE32_UPPER: char = 'B';

/// Produce a fully attested node: fill `attributedTo` (from the identity's
/// `did:atomic`), compute the content hash, sign, and attach the proof.
pub fn attest(mut node: CanonicalNode, identity: &Identity, keypair: &KeyPair) -> CanonicalNode {
    let did = did::did_for_public_key(&identity.public_key);
    if node.attributed_to.is_none() {
        node.attributed_to = Some(did.clone());
    }

    // Hash first (over node without proof/contentHash), then sign (over node
    // with contentHash, without proof).
    node.content_hash = Some(node.compute_content_hash());

    let signature = Signer::new(keypair).sign(&node.signing_bytes());
    let proof_value = encode_proof_value(&signature);

    node.proof = Some(Proof {
        type_: PROOF_TYPE.to_string(),
        cryptosuite: CRYPTOSUITE.to_string(),
        verification_method: did::verification_method(&did),
        proof_purpose: PROOF_PURPOSE.to_string(),
        proof_value,
    });
    node
}

/// Verify an attested node against a public key: the content hash must
/// recompute, and the signature must verify over the canonical signing bytes.
pub fn verify(node: &CanonicalNode, public_key: &PublicKey) -> Result<()> {
    // 1. Content hash integrity.
    let expected = node
        .content_hash
        .as_ref()
        .ok_or_else(|| CanonicalError::Proof("node carries no contentHash".into()))?;
    let actual = node.compute_content_hash();
    if *expected != actual {
        return Err(CanonicalError::HashMismatch {
            expected: expected.clone(),
            actual,
        });
    }

    // 2. Signature over the canonical signing bytes.
    let proof = node
        .proof
        .as_ref()
        .ok_or_else(|| CanonicalError::Proof("node carries no proof".into()))?;
    let signature = decode_proof_value(&proof.proof_value)?;
    signature
        .verify(&node.signing_bytes(), public_key)
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

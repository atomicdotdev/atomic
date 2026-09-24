//! Change signing — Ed25519 signatures over a change's content hash.
//!
//! This module provides the cryptographic core of change signing:
//! - [`sign_change`]: sign a change's content hash with a secret key
//! - [`verify_change_signature`]: verify a signature with a *caller-supplied*
//!   public key
//!
//! # Trust Model (critical)
//!
//! The public key used for verification MUST come from outside the change
//! being verified — an identity store lookup, `identity lookup-key`, or a
//! caller argument. The `signer_did` embedded in the signature is a
//! *discovery hint*, never a trust root: an attacker can sign garbage with
//! their own keypair and embed their own DID. Self-referential verification
//! (checking the signature against the key carried in the change) validates
//! nothing and is deliberately not provided here.

use crate::change::format_v3::ChangeSignature;
use crate::types::Hash;

/// Domain separator for change signatures. Ed25519 is deterministic, so the
/// domain keeps a change signature from being transplanted into (or accepted
/// from) a signature made over a different object kind (attestation, intent,
/// capture) with the same key.
pub const CHANGE_SIGNATURE_DOMAIN: &[u8] = b"atomic.change.signature.v1";

/// Sign a change's content hash with an Ed25519 secret key.
///
/// `secret_key_bytes` is the 32-byte Ed25519 seed
/// (`atomic_identity::SecretKey::as_bytes`). `signer_did` is the signer's
/// `did:atomic:...` fingerprint (see `atomic_canonical::did`).
///
/// The signature is made over `domain || 0 || content_hash`, so a signature
/// over a raw hash (or another object's signature input) never verifies as a
/// change signature.
pub fn sign_change(
    signer_did: &str,
    secret_key_bytes: &[u8; 32],
    content_hash: &Hash,
    timestamp: i64,
) -> ChangeSignature {
    use ed25519_dalek::Signer;

    let signing_key = ed25519_dalek::SigningKey::from_bytes(secret_key_bytes);
    let message = signature_message(content_hash);
    let signature = signing_key.sign(&message).to_bytes();

    ChangeSignature::new(signer_did, signature, *content_hash.as_bytes(), timestamp)
}

/// Verify a change signature against a *caller-supplied* Ed25519 public key.
///
/// The caller resolves the key out-of-band (identity store, lookup-key
/// remote, explicit argument). This function deliberately takes no key from
/// the signature itself — self-referential verification is the broken trust
/// model this module exists to avoid.
///
/// # Arguments
///
/// * `signature` — the signature section from the change.
/// * `public_key_bytes` — the verifier's *known* Ed25519 public key for the
///   claimed signer.
/// * `content_hash` — the change's verified content hash (e.g. from the
///   trailer after `Change::deserialize`).
///
/// # Errors
///
/// Returns an error if the signature was not made over this change's hash
/// with the matching private key, or if the signature section is malformed
/// (wrong method, hash mismatch with the change's actual hash).
pub fn verify_change_signature(
    signature: &ChangeSignature,
    public_key_bytes: &[u8; 32],
    content_hash: &Hash,
) -> Result<(), ChangeSignatureError> {
    use ed25519_dalek::Verifier;

    if !signature.is_ed25519() {
        return Err(ChangeSignatureError::UnsupportedMethod {
            method: signature.method.clone(),
        });
    }

    // The signed hash must be THIS change's hash — a signature over some
    // other change's hash is meaningless even if it verifies cryptographically.
    if signature.signed_hash != *content_hash.as_bytes() {
        return Err(ChangeSignatureError::HashMismatch {
            signed: baseline32(&signature.signed_hash),
            actual: crate::types::Base32::to_base32(content_hash),
        });
    }

    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(public_key_bytes)
        .map_err(|_| ChangeSignatureError::InvalidPublicKey)?;

    let sig = ed25519_dalek::Signature::from_bytes(&signature.signature);
    verifying_key
        .verify(&signature_message(content_hash), &sig)
        .map_err(|_| ChangeSignatureError::SignatureVerificationFailed {
            signer: signature.signer_did.clone(),
        })
}

/// The canonical signed message: domain separator, then the content hash.
fn signature_message(content_hash: &Hash) -> Vec<u8> {
    let mut message = Vec::with_capacity(CHANGE_SIGNATURE_DOMAIN.len() + 1 + 32);
    message.extend_from_slice(CHANGE_SIGNATURE_DOMAIN);
    message.push(0);
    message.extend_from_slice(content_hash.as_bytes());
    message
}

/// Base32-encode raw bytes for error messages.
fn baseline32(bytes: &[u8; 32]) -> String {
    data_encoding::BASE32_NOPAD.encode(bytes)
}

/// Errors from change signature verification.
#[derive(Debug, thiserror::Error)]
pub enum ChangeSignatureError {
    /// The signature section uses a method this version does not verify.
    #[error("unsupported signature method: `{method}`")]
    UnsupportedMethod {
        /// The unsupported method identifier.
        method: String,
    },

    /// The signature was made over a different change's hash.
    #[error("signature was made over hash {signed}, but this change's hash is {actual}")]
    HashMismatch {
        /// The hash the signature actually covers.
        signed: String,
        /// This change's actual content hash.
        actual: String,
    },

    /// The caller-supplied public key is not a valid Ed25519 key.
    #[error("invalid public key")]
    InvalidPublicKey,

    /// The Ed25519 signature does not verify against the supplied key.
    #[error("signature verification failed for signer `{signer}` (key was resolved out-of-band)")]
    SignatureVerificationFailed {
        /// The DID claimed by the signature.
        signer: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use rand::RngCore;

    fn test_keypair() -> (SigningKey, [u8; 32]) {
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        (signing_key, seed)
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let (signing_key, seed) = test_keypair();
        let hash = Hash::of(b"test change content");
        let did = "did:atomic:TESTSIGNER";

        let sig = sign_change(did, &seed, &hash, 1739290034);
        assert!(sig.is_ed25519());
        assert_eq!(sig.signer_did, did);
        assert_eq!(sig.signed_hash, *hash.as_bytes());

        let public = signing_key.verifying_key().to_bytes();
        verify_change_signature(&sig, &public, &hash).unwrap();
    }

    #[test]
    fn tampered_hash_fails() {
        let (signing_key, seed) = test_keypair();
        let hash = Hash::of(b"original");
        let sig = sign_change("did:atomic:x", &seed, &hash, 0);

        let other = Hash::of(b"tampered");
        let public = signing_key.verifying_key().to_bytes();
        let err = verify_change_signature(&sig, &public, &other).unwrap_err();
        assert!(matches!(err, ChangeSignatureError::HashMismatch { .. }));
    }

    #[test]
    fn wrong_key_fails() {
        let (_signing_key, seed) = test_keypair();
        let (_other_key, other_seed) = test_keypair();
        let hash = Hash::of(b"content");
        let sig = sign_change("did:atomic:attacker", &seed, &hash, 0);

        // Verify against a DIFFERENT (victim's) key — the forge scenario.
        let victim_key = SigningKey::from_bytes(&other_seed).verifying_key().to_bytes();

        let err = verify_change_signature(&sig, &victim_key, &hash).unwrap_err();
        assert!(matches!(
            err,
            ChangeSignatureError::SignatureVerificationFailed { .. }
        ));
    }

    #[test]
    fn domain_separation() {
        // A signature made over the raw hash must NOT verify as a change
        // signature (domain separation).
        let (signing_key, _seed) = test_keypair();
        let hash = Hash::of(b"content");
        let raw_sig = signing_key.sign(hash.as_bytes()).to_bytes();

        let sig = ChangeSignature::new(
            "did:atomic:x",
            raw_sig,
            *hash.as_bytes(),
            0,
        );
        let public = signing_key.verifying_key().to_bytes();
        let err = verify_change_signature(&sig, &public, &hash).unwrap_err();
        assert!(matches!(
            err,
            ChangeSignatureError::SignatureVerificationFailed { .. }
        ));
    }
}

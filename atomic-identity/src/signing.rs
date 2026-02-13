//! Change signing and verification
//!
//! This module provides functionality for signing and verifying changes
//! using Ed25519 signatures. Signatures prove that a change was created
//! by the holder of a specific identity's secret key.
//!
//! # Overview
//!
//! - **Signing**: Create a signature for change data using a secret key
//! - **Verification**: Verify a signature against a public key
//! - **Detached Signatures**: Signatures stored separately from the data
//!
//! # Example
//!
//! ```rust
//! use atomic_identity::{Identity, KeyPair};
//! use atomic_identity::signing::{Signer, SignedData, Signature};
//!
//! // Create a keypair and identity
//! let keypair = KeyPair::generate();
//! let identity = Identity::new("alice", &keypair);
//!
//! // Sign some data
//! let data = b"Change content to sign";
//! let signer = Signer::new(&keypair);
//! let signature = signer.sign(data);
//!
//! // Verify the signature
//! assert!(signature.verify(data, &identity.public_key).is_ok());
//!
//! // Create signed data (data + signature together)
//! let signed = SignedData::sign(data.to_vec(), &keypair);
//! assert!(signed.verify(&identity.public_key).is_ok());
//! ```

use crate::identity::{Identity, IdentityId};
use crate::keypair::{KeyPair, PublicKey, SecretKey};
use crate::IdentityError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Size of an Ed25519 signature in bytes.
pub const SIGNATURE_SIZE: usize = 64;

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature([u8; SIGNATURE_SIZE]);

impl Signature {
    /// Create a signature from raw bytes.
    pub fn from_bytes(bytes: [u8; SIGNATURE_SIZE]) -> Self {
        Signature(bytes)
    }

    /// Create a signature from a byte slice.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, IdentityError> {
        if bytes.len() != SIGNATURE_SIZE {
            return Err(IdentityError::InvalidSignature);
        }

        let mut arr = [0u8; SIGNATURE_SIZE];
        arr.copy_from_slice(bytes);
        Ok(Signature(arr))
    }

    /// Get the raw bytes of the signature.
    pub fn as_bytes(&self) -> &[u8; SIGNATURE_SIZE] {
        &self.0
    }

    /// Encode the signature as base32.
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.0)
    }

    /// Decode a signature from base32.
    pub fn from_base32(s: &str) -> Result<Self, IdentityError> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|_| IdentityError::InvalidBase32)?;

        Self::from_slice(&bytes)
    }

    /// Verify this signature against a message and public key.
    pub fn verify(&self, message: &[u8], public_key: &PublicKey) -> Result<(), IdentityError> {
        public_key.verify(message, &self.0)
    }

    /// Verify this signature against a message and identity.
    pub fn verify_with_identity(
        &self,
        message: &[u8],
        identity: &Identity,
    ) -> Result<(), IdentityError> {
        self.verify(message, &identity.public_key)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({}...)", &self.to_base32()[..12])
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_base32())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            Signature::from_base32(&s).map_err(serde::de::Error::custom)
        } else {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            Signature::from_slice(&bytes).map_err(serde::de::Error::custom)
        }
    }
}

/// A signer that can create signatures using a secret key.
pub struct Signer<'a> {
    /// The secret key used for signing.
    secret_key: &'a SecretKey,
}

impl<'a> Signer<'a> {
    /// Create a new signer from a secret key.
    pub fn from_secret_key(secret_key: &'a SecretKey) -> Self {
        Self { secret_key }
    }

    /// Create a new signer from a keypair.
    pub fn new(keypair: &'a KeyPair) -> Self {
        Self {
            secret_key: &keypair.secret,
        }
    }

    /// Sign a message.
    pub fn sign(&self, message: &[u8]) -> Signature {
        Signature(self.secret_key.sign(message))
    }

    /// Sign a message and return signed data.
    pub fn sign_data(&self, data: Vec<u8>) -> SignedData {
        let signature = self.sign(&data);
        SignedData { data, signature }
    }
}

/// Data with an attached signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedData {
    /// The signed data.
    pub data: Vec<u8>,

    /// The signature over the data.
    pub signature: Signature,
}

impl SignedData {
    /// Create signed data from raw bytes and a keypair.
    pub fn sign(data: Vec<u8>, keypair: &KeyPair) -> Self {
        let signer = Signer::new(keypair);
        signer.sign_data(data)
    }

    /// Verify the signature using a public key.
    pub fn verify(&self, public_key: &PublicKey) -> Result<(), IdentityError> {
        self.signature.verify(&self.data, public_key)
    }

    /// Verify the signature using an identity.
    pub fn verify_with_identity(&self, identity: &Identity) -> Result<(), IdentityError> {
        self.signature.verify_with_identity(&self.data, identity)
    }

    /// Get the data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consume and return the data.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Metadata about a signature, including who signed and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// The signer's identity ID.
    pub signer_id: IdentityId,

    /// The signer's public key (for verification without looking up identity).
    pub public_key: PublicKey,

    /// The signer's name (for display).
    pub signer_name: String,

    /// When the signature was created.
    pub signed_at: DateTime<Utc>,

    /// The signature itself.
    pub signature: Signature,

    /// Optional reason or context for the signature.
    #[serde(default)]
    pub reason: Option<String>,
}

impl SignatureInfo {
    /// Create signature info from an identity and message.
    pub fn sign(
        message: &[u8],
        identity: &Identity,
        keypair: &KeyPair,
        reason: Option<String>,
    ) -> Self {
        let signer = Signer::new(keypair);
        let signature = signer.sign(message);

        Self {
            signer_id: identity.id,
            public_key: identity.public_key.clone(),
            signer_name: identity.name.clone(),
            signed_at: Utc::now(),
            signature,
            reason,
        }
    }

    /// Verify this signature against a message.
    pub fn verify(&self, message: &[u8]) -> Result<(), IdentityError> {
        self.signature.verify(message, &self.public_key)
    }

    /// Verify this signature against a message and ensure the signer matches.
    pub fn verify_signer(
        &self,
        message: &[u8],
        expected_signer: &IdentityId,
    ) -> Result<(), IdentityError> {
        if self.signer_id != *expected_signer {
            return Err(IdentityError::VerificationFailed(
                "Signer does not match expected identity".to_string(),
            ));
        }
        self.verify(message)
    }
}

impl fmt::Display for SignatureInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Signed by {} at {}",
            self.signer_name,
            self.signed_at.format("%Y-%m-%d %H:%M:%S UTC")
        )
    }
}

/// A collection of signatures (for multi-party signing).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureSet {
    /// The signatures in this set.
    pub signatures: Vec<SignatureInfo>,
}

impl SignatureSet {
    /// Create an empty signature set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a signature to the set.
    pub fn add(&mut self, signature: SignatureInfo) {
        // Avoid duplicates by signer ID
        if !self.signatures.iter().any(|s| s.signer_id == signature.signer_id) {
            self.signatures.push(signature);
        }
    }

    /// Sign and add to the set.
    pub fn sign_and_add(
        &mut self,
        message: &[u8],
        identity: &Identity,
        keypair: &KeyPair,
        reason: Option<String>,
    ) {
        let signature = SignatureInfo::sign(message, identity, keypair, reason);
        self.add(signature);
    }

    /// Verify all signatures in the set.
    pub fn verify_all(&self, message: &[u8]) -> Result<(), IdentityError> {
        for signature in &self.signatures {
            signature.verify(message)?;
        }
        Ok(())
    }

    /// Check if a specific identity has signed.
    pub fn is_signed_by(&self, signer_id: &IdentityId) -> bool {
        self.signatures.iter().any(|s| &s.signer_id == signer_id)
    }

    /// Get the signature from a specific identity.
    pub fn get_signature(&self, signer_id: &IdentityId) -> Option<&SignatureInfo> {
        self.signatures.iter().find(|s| &s.signer_id == signer_id)
    }

    /// Get the number of signatures.
    pub fn len(&self) -> usize {
        self.signatures.len()
    }

    /// Check if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }

    /// Get all signer IDs.
    pub fn signer_ids(&self) -> Vec<IdentityId> {
        self.signatures.iter().map(|s| s.signer_id).collect()
    }

    /// Get all signer names.
    pub fn signer_names(&self) -> Vec<&str> {
        self.signatures.iter().map(|s| s.signer_name.as_str()).collect()
    }
}

/// Verification result with details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationResult {
    /// Signature is valid.
    Valid {
        /// The identity that signed.
        signer_id: IdentityId,
        /// When the signature was made.
        signed_at: DateTime<Utc>,
    },

    /// Signature is invalid.
    Invalid {
        /// The reason for invalidity.
        reason: String,
    },

    /// No signature present.
    NotSigned,
}

impl VerificationResult {
    /// Check if the verification succeeded.
    pub fn is_valid(&self) -> bool {
        matches!(self, VerificationResult::Valid { .. })
    }

    /// Check if the verification failed.
    pub fn is_invalid(&self) -> bool {
        matches!(self, VerificationResult::Invalid { .. })
    }

    /// Check if no signature was present.
    pub fn is_not_signed(&self) -> bool {
        matches!(self, VerificationResult::NotSigned)
    }

    /// Convert to a Result.
    pub fn to_result(&self) -> Result<(), IdentityError> {
        match self {
            VerificationResult::Valid { .. } => Ok(()),
            VerificationResult::Invalid { reason } => {
                Err(IdentityError::VerificationFailed(reason.clone()))
            }
            VerificationResult::NotSigned => {
                Err(IdentityError::VerificationFailed("Not signed".to_string()))
            }
        }
    }
}

impl fmt::Display for VerificationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerificationResult::Valid { signer_id, signed_at } => {
                write!(
                    f,
                    "Valid signature by {} at {}",
                    signer_id.short(),
                    signed_at.format("%Y-%m-%d %H:%M:%S UTC")
                )
            }
            VerificationResult::Invalid { reason } => {
                write!(f, "Invalid signature: {}", reason)
            }
            VerificationResult::NotSigned => {
                write!(f, "Not signed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_roundtrip() {
        let keypair = KeyPair::generate();
        let message = b"Hello, Atomic!";

        let signer = Signer::new(&keypair);
        let signature = signer.sign(message);

        assert!(signature.verify(message, &keypair.public).is_ok());
    }

    #[test]
    fn test_signature_invalid_message() {
        let keypair = KeyPair::generate();
        let message = b"Hello, Atomic!";
        let wrong_message = b"Goodbye, Atomic!";

        let signer = Signer::new(&keypair);
        let signature = signer.sign(message);

        assert!(signature.verify(wrong_message, &keypair.public).is_err());
    }

    #[test]
    fn test_signature_base32_roundtrip() {
        let keypair = KeyPair::generate();
        let message = b"Test message";

        let signature = Signer::new(&keypair).sign(message);
        let base32 = signature.to_base32();
        let recovered = Signature::from_base32(&base32).unwrap();

        assert_eq!(signature, recovered);
    }

    #[test]
    fn test_signed_data() {
        let keypair = KeyPair::generate();
        let data = b"Important data".to_vec();

        let signed = SignedData::sign(data.clone(), &keypair);

        assert_eq!(signed.data(), data.as_slice());
        assert!(signed.verify(&keypair.public).is_ok());
    }

    #[test]
    fn test_signed_data_into_data() {
        let keypair = KeyPair::generate();
        let data = b"Important data".to_vec();

        let signed = SignedData::sign(data.clone(), &keypair);
        let recovered_data = signed.into_data();

        assert_eq!(recovered_data, data);
    }

    #[test]
    fn test_signature_info() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("alice", &keypair);
        let message = b"Message to sign";

        let info = SignatureInfo::sign(message, &identity, &keypair, Some("Test reason".to_string()));

        assert_eq!(info.signer_id, identity.id);
        assert_eq!(info.signer_name, "alice");
        assert!(info.reason.is_some());
        assert!(info.verify(message).is_ok());
    }

    #[test]
    fn test_signature_info_verify_signer() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("alice", &keypair);
        let message = b"Message to sign";

        let info = SignatureInfo::sign(message, &identity, &keypair, None);

        assert!(info.verify_signer(message, &identity.id).is_ok());

        // Wrong signer
        let other_identity = Identity::generate("bob");
        assert!(info.verify_signer(message, &other_identity.id).is_err());
    }

    #[test]
    fn test_signature_set() {
        let keypair1 = KeyPair::generate();
        let identity1 = Identity::new("alice", &keypair1);

        let keypair2 = KeyPair::generate();
        let identity2 = Identity::new("bob", &keypair2);

        let message = b"Multi-party message";
        let mut set = SignatureSet::new();

        assert!(set.is_empty());

        set.sign_and_add(message, &identity1, &keypair1, None);
        set.sign_and_add(message, &identity2, &keypair2, None);

        assert_eq!(set.len(), 2);
        assert!(set.is_signed_by(&identity1.id));
        assert!(set.is_signed_by(&identity2.id));
        assert!(set.verify_all(message).is_ok());
    }

    #[test]
    fn test_signature_set_no_duplicates() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("alice", &keypair);
        let message = b"Message";

        let mut set = SignatureSet::new();
        set.sign_and_add(message, &identity, &keypair, None);
        set.sign_and_add(message, &identity, &keypair, None); // Duplicate

        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_verification_result() {
        let valid = VerificationResult::Valid {
            signer_id: Identity::generate("test").id,
            signed_at: Utc::now(),
        };
        assert!(valid.is_valid());
        assert!(valid.to_result().is_ok());

        let invalid = VerificationResult::Invalid {
            reason: "Bad signature".to_string(),
        };
        assert!(invalid.is_invalid());
        assert!(invalid.to_result().is_err());

        let not_signed = VerificationResult::NotSigned;
        assert!(not_signed.is_not_signed());
        assert!(not_signed.to_result().is_err());
    }

    #[test]
    fn test_signature_json_roundtrip() {
        let keypair = KeyPair::generate();
        let signature = Signer::new(&keypair).sign(b"test");

        let json = serde_json::to_string(&signature).unwrap();
        let recovered: Signature = serde_json::from_str(&json).unwrap();

        assert_eq!(signature, recovered);
    }

    #[test]
    fn test_signed_data_json_roundtrip() {
        let keypair = KeyPair::generate();
        let signed = SignedData::sign(b"test data".to_vec(), &keypair);

        let json = serde_json::to_string(&signed).unwrap();
        let recovered: SignedData = serde_json::from_str(&json).unwrap();

        assert_eq!(signed, recovered);
        assert!(recovered.verify(&keypair.public).is_ok());
    }

    #[test]
    fn test_signature_info_json_roundtrip() {
        let keypair = KeyPair::generate();
        let identity = Identity::new("alice", &keypair);
        let info = SignatureInfo::sign(b"message", &identity, &keypair, None);

        let json = serde_json::to_string(&info).unwrap();
        let recovered: SignatureInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(info.signer_id, recovered.signer_id);
        assert_eq!(info.signature, recovered.signature);
    }

    #[test]
    fn test_signer_from_secret_key() {
        let keypair = KeyPair::generate();
        let signer = Signer::from_secret_key(&keypair.secret);

        let message = b"Test";
        let signature = signer.sign(message);

        assert!(signature.verify(message, &keypair.public).is_ok());
    }
}

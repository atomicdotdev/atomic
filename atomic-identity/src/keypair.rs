//! Ed25519 key pair generation and management
//!
//! This module provides cryptographic identity functionality for Atomic,
//! including key generation, signing, and verification.

use ed25519_dalek::{SigningKey, VerifyingKey, Signature, Signer, Verifier};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::IdentityError;

/// A public key for verifying signatures.
///
/// Public keys can be freely shared and are used to verify that
/// changes were signed by the corresponding secret key holder.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Size of the public key in bytes
    pub const SIZE: usize = 32;

    /// Create a public key from raw bytes
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, IdentityError> {
        let key = VerifyingKey::from_bytes(bytes)
            .map_err(|e| IdentityError::InvalidKey(e.to_string()))?;
        Ok(PublicKey(key))
    }

    /// Get the raw bytes of the public key
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Verify a signature against a message
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), IdentityError> {
        let sig = Signature::from_bytes(signature);
        self.0
            .verify(message, &sig)
            .map_err(|e| IdentityError::VerificationFailed(e.to_string()))
    }

    /// Encode the public key as base32
    pub fn to_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(self.as_bytes())
    }

    /// Decode a public key from base32
    pub fn from_base32(s: &str) -> Result<Self, IdentityError> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|e| IdentityError::InvalidKey(format!("Invalid base32: {}", e)))?;

        if bytes.len() != 32 {
            return Err(IdentityError::InvalidKey(format!(
                "Expected 32 bytes, got {}",
                bytes.len()
            )));
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Self::from_bytes(&arr)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", &self.to_base32()[..12])
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

impl Serialize for PublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_base32())
        } else {
            serializer.serialize_bytes(self.as_bytes())
        }
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            PublicKey::from_base32(&s).map_err(serde::de::Error::custom)
        } else {
            let bytes = <[u8; 32]>::deserialize(deserializer)?;
            PublicKey::from_bytes(&bytes).map_err(serde::de::Error::custom)
        }
    }
}

/// A secret key for signing changes.
///
/// Secret keys must be kept private. They are used to sign changes
/// to prove authorship.
pub struct SecretKey(SigningKey);

impl SecretKey {
    /// Size of the secret key in bytes
    pub const SIZE: usize = 32;

    /// Generate a new random secret key
    pub fn generate() -> Self {
        let mut secret_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut secret_bytes);
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        SecretKey(signing_key)
    }

    /// Create a secret key from raw bytes
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        SecretKey(SigningKey::from_bytes(bytes))
    }

    /// Get the raw bytes of the secret key
    ///
    /// # Security
    ///
    /// The returned bytes are sensitive and should be handled carefully.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Get the corresponding public key
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Sign a message
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let sig = self.0.sign(message);
        sig.to_bytes()
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey([REDACTED])")
    }
}

// Intentionally not implementing Clone, Serialize, or Display for SecretKey
// to prevent accidental exposure of secret key material.

/// A key pair consisting of a secret key and its corresponding public key.
pub struct KeyPair {
    /// The secret key (for signing)
    pub secret: SecretKey,
    /// The public key (for verification)
    pub public: PublicKey,
}

impl KeyPair {
    /// Generate a new random key pair
    pub fn generate() -> Self {
        let secret = SecretKey::generate();
        let public = secret.public_key();
        KeyPair { secret, public }
    }

    /// Create a key pair from a secret key
    pub fn from_secret_key(secret: SecretKey) -> Self {
        let public = secret.public_key();
        KeyPair { secret, public }
    }

    /// Sign a message with the secret key
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.secret.sign(message)
    }

    /// Verify a signature with the public key
    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), IdentityError> {
        self.public.verify(message, signature)
    }
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair")
            .field("public", &self.public)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_generation() {
        let keypair = KeyPair::generate();
        assert_eq!(keypair.public.as_bytes().len(), 32);
    }

    #[test]
    fn test_sign_and_verify() {
        let keypair = KeyPair::generate();
        let message = b"Hello, Atomic!";

        let signature = keypair.sign(message);
        assert!(keypair.verify(message, &signature).is_ok());
    }

    #[test]
    fn test_verify_wrong_message() {
        let keypair = KeyPair::generate();
        let message = b"Hello, Atomic!";
        let wrong_message = b"Goodbye, Atomic!";

        let signature = keypair.sign(message);
        assert!(keypair.verify(wrong_message, &signature).is_err());
    }

    #[test]
    fn test_public_key_base32_roundtrip() {
        let keypair = KeyPair::generate();
        let base32 = keypair.public.to_base32();
        let recovered = PublicKey::from_base32(&base32).unwrap();
        assert_eq!(keypair.public, recovered);
    }

    #[test]
    fn test_public_key_serialization() {
        let keypair = KeyPair::generate();
        let json = serde_json::to_string(&keypair.public).unwrap();
        let parsed: PublicKey = serde_json::from_str(&json).unwrap();
        assert_eq!(keypair.public, parsed);
    }

    #[test]
    fn test_secret_key_from_bytes() {
        let keypair1 = KeyPair::generate();
        let bytes = keypair1.secret.as_bytes();
        let secret2 = SecretKey::from_bytes(bytes);

        // Same secret key should produce same signatures
        let message = b"test message";
        let sig1 = keypair1.sign(message);
        let sig2 = secret2.sign(message);
        assert_eq!(sig1, sig2);
    }
}

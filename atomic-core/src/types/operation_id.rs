//! Content-addressed identity for an immutable repository operation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{Base32, Hash};

const OPERATION_ID_DOMAIN: &[u8] = b"atomic:operation:v1\0";

/// Blake3 identity of an operation's complete immutable canonical payload.
///
/// The derived ID itself, mutable operation heads, effect receipts, and any
/// execution phase are not part of the payload.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId([u8; 32]);

impl OperationId {
    /// Encoded size of an operation identity.
    pub const SIZE: usize = 32;

    /// Construct an identity from canonical bytes.
    #[inline]
    pub const fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
        Self(bytes)
    }

    /// Borrow the canonical byte representation.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; Self::SIZE] {
        &self.0
    }

    /// Hash a complete immutable canonical operation payload.
    pub fn from_canonical_bytes(payload: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(OPERATION_ID_DOMAIN);
        hasher.update(payload);
        Self(hasher.finalize().into())
    }

    /// Encode as lowercase hexadecimal.
    pub fn to_hex(&self) -> String {
        Hash::from_bytes(self.0).to_hex()
    }

    /// Decode a full lowercase or uppercase hexadecimal identity.
    pub fn from_hex(value: &str) -> Option<Self> {
        Hash::from_hex(value).map(|hash| Self::from_bytes(*hash.as_bytes()))
    }
}

impl Base32 for OperationId {
    fn to_base32(&self) -> String {
        Hash::from_bytes(self.0).to_base32()
    }

    fn from_base32(value: &[u8]) -> Option<Self> {
        Hash::from_base32(value).map(|hash| Self::from_bytes(*hash.as_bytes()))
    }
}

impl FromStr for OperationId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_base32(value.as_bytes()).ok_or("invalid operation ID")
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_base32())
    }
}

impl fmt::Debug for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("OperationId")
            .field(&self.to_base32())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_roundtrips_bytes_base32_and_hex() {
        let id = OperationId::from_canonical_bytes(b"operation");
        assert_eq!(OperationId::from_bytes(*id.as_bytes()), id);
        assert_eq!(
            OperationId::from_base32(id.to_base32().as_bytes()),
            Some(id)
        );
        assert_eq!(OperationId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(id.to_string().parse::<OperationId>(), Ok(id));
    }

    #[test]
    fn operation_id_hashes_the_complete_payload() {
        assert_ne!(
            OperationId::from_canonical_bytes(b"before+delta+a"),
            OperationId::from_canonical_bytes(b"before+delta+b")
        );
    }
}

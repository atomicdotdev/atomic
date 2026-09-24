//! Content-addressed identity for an immutable external-effect receipt.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{Base32, Hash};

const EFFECT_RECEIPT_ID_DOMAIN: &[u8] = b"atomic:effect-receipt:v1\0";

/// Blake3 identity of a receipt's complete immutable canonical payload.
///
/// The derived ID itself is excluded from that payload. A receipt may refer to
/// an operation, but operations never refer to receipts, keeping the hash graph
/// acyclic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectReceiptId([u8; 32]);

impl EffectReceiptId {
    /// Encoded size of an effect-receipt identity.
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

    /// Hash a complete immutable canonical effect-receipt payload.
    pub fn from_canonical_bytes(payload: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(EFFECT_RECEIPT_ID_DOMAIN);
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

impl Base32 for EffectReceiptId {
    fn to_base32(&self) -> String {
        Hash::from_bytes(self.0).to_base32()
    }

    fn from_base32(value: &[u8]) -> Option<Self> {
        Hash::from_base32(value).map(|hash| Self::from_bytes(*hash.as_bytes()))
    }
}

impl FromStr for EffectReceiptId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_base32(value.as_bytes()).ok_or("invalid effect receipt ID")
    }
}

impl fmt::Display for EffectReceiptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_base32())
    }
}

impl fmt::Debug for EffectReceiptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("EffectReceiptId")
            .field(&self.to_base32())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OperationId;

    #[test]
    fn effect_receipt_id_roundtrips_bytes_base32_and_hex() {
        let id = EffectReceiptId::from_canonical_bytes(b"receipt");
        assert_eq!(EffectReceiptId::from_bytes(*id.as_bytes()), id);
        assert_eq!(
            EffectReceiptId::from_base32(id.to_base32().as_bytes()),
            Some(id)
        );
        assert_eq!(EffectReceiptId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(id.to_string().parse::<EffectReceiptId>(), Ok(id));
    }

    #[test]
    fn receipt_and_operation_domains_are_distinct() {
        let payload = b"same canonical payload";
        assert_ne!(
            EffectReceiptId::from_canonical_bytes(payload).as_bytes(),
            OperationId::from_canonical_bytes(payload).as_bytes()
        );
    }
}

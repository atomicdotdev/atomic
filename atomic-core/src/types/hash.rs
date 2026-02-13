//! Hash types for content addressing and state tracking
//!
//! This module defines the cryptographic hash types used throughout Atomic.
//!
//! # Design Decision: Unified Hash Type
//!
//! Following the original Atomic project, we consolidate `Hash` as a type alias
//! for `Merkle`. This simplifies the codebase by having a single hash type
//! throughout, while still using Blake3 for fast, secure hashing.
//!
//! Benefits:
//! - Single unified type system
//! - Simpler API - no need to convert between Hash and Merkle
//! - Future-compatible with cryptographic proof systems if needed

use super::Base32;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Merkle hash (32 bytes) - the unified hash type for Atomic.
///
/// Merkle is used for both content addressing (identifying changes) and
/// incremental state tracking (channel state). It uses Blake3 internally
/// for fast, secure hashing.
///
/// # Content Addressing
///
/// Changes are uniquely identified by hashing their content:
/// ```text
/// hash = Blake3(change_content)
/// ```
///
/// # Incremental State
///
/// Channel state is computed incrementally:
/// ```text
/// state_0 = Blake3(empty)
/// state_n = Blake3(state_{n-1} || change_hash_n)
/// ```
///
/// This allows efficient comparison of channel states and detection of
/// divergence points during synchronization.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Merkle(pub [u8; 32]);

/// Hash is a type alias for Merkle.
///
/// This follows the original Atomic project's design decision to use a single
/// unified hash type throughout the codebase. Both content hashes (for changes)
/// and state hashes (for channels) use the same type.
pub type Hash = Merkle;

/// Hasher for computing Merkle/Hash values.
///
/// This wraps Blake3 to provide a consistent hashing interface.
pub struct Hasher {
    inner: blake3::Hasher,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    /// Create a new hasher
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    /// Add data to the hash computation
    #[inline]
    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.inner.update(data);
        self
    }

    /// Finalize and return the hash
    #[inline]
    pub fn finalize(&self) -> Merkle {
        Merkle(self.inner.finalize().into())
    }
}

impl Merkle {
    /// The zero hash (all zeros) - represents "none" or "empty"
    pub const ZERO: Merkle = Merkle([0u8; 32]);

    /// Alias for ZERO for API compatibility
    pub const NONE: Merkle = Self::ZERO;

    /// Hash size in bytes
    pub const SIZE: usize = 32;

    /// Create a Merkle from raw bytes
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Merkle(bytes)
    }

    /// Get the underlying bytes
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compute a hash of the given data
    #[inline]
    pub fn of(data: &[u8]) -> Self {
        Merkle(blake3::hash(data).into())
    }

    /// Compute the initial state (hash of empty input)
    #[inline]
    pub fn initial() -> Self {
        Self::of(&[])
    }

    /// Compute the next state by incorporating another hash.
    ///
    /// This is the core operation for incremental Merkle state:
    /// `next_state = Hash(current_state || change_hash)`
    pub fn next(&self, change_hash: &Merkle) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(&self.0);
        hasher.update(&change_hash.0);
        hasher.finalize()
    }

    /// Check if this is the zero/none hash
    #[inline]
    pub fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }

    /// Alias for is_zero for API compatibility
    #[inline]
    pub fn is_none(&self) -> bool {
        self.is_zero()
    }

    /// Convert to a hex string (lowercase)
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in &self.0 {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    /// Parse from a hex string
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }

        let mut bytes = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex_str = std::str::from_utf8(chunk).ok()?;
            bytes[i] = u8::from_str_radix(hex_str, 16).ok()?;
        }
        Some(Merkle(bytes))
    }

    /// Parse from a base32 prefix (for partial matching)
    pub fn from_prefix(s: &str) -> Option<Self> {
        // For prefix matching, we need at least some characters
        if s.is_empty() || s.len() > 52 {
            return None;
        }

        // Pad with 'A' (zero bits) to make a full hash
        let mut padded = s.to_uppercase();
        while padded.len() < 52 {
            padded.push('A');
        }

        Self::from_base32(padded.as_bytes())
    }
}

impl fmt::Debug for Merkle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..12])
    }
}

impl fmt::Display for Merkle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

impl Serialize for Merkle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_base32())
        } else {
            // Use newtype serialization for binary formats
            serializer.serialize_newtype_struct("Merkle", &self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Merkle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            Merkle::from_base32(s.as_bytes())
                .ok_or_else(|| serde::de::Error::custom("invalid base32 hash"))
        } else {
            // Deserialize as newtype struct
            struct MerkleVisitor;

            impl<'de> serde::de::Visitor<'de> for MerkleVisitor {
                type Value = Merkle;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("a 32-byte hash")
                }

                fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    let bytes = <[u8; 32]>::deserialize(deserializer)?;
                    Ok(Merkle(bytes))
                }

                fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                where
                    A: serde::de::SeqAccess<'de>,
                {
                    let mut bytes = [0u8; 32];
                    for (i, byte) in bytes.iter_mut().enumerate() {
                        *byte = seq
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                    }
                    Ok(Merkle(bytes))
                }
            }

            deserializer.deserialize_newtype_struct("Merkle", MerkleVisitor)
        }
    }
}

// Base32 alphabet (RFC 4648, no padding)
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl Base32 for Merkle {
    fn to_base32(&self) -> String {
        // 32 bytes -> 52 base32 characters (ceiling of 32*8/5)
        let mut result = String::with_capacity(52);
        let mut bits: u64 = 0;
        let mut num_bits = 0;

        for &byte in &self.0 {
            bits = (bits << 8) | (byte as u64);
            num_bits += 8;

            while num_bits >= 5 {
                num_bits -= 5;
                let idx = ((bits >> num_bits) & 0x1f) as usize;
                result.push(BASE32_ALPHABET[idx] as char);
            }
        }

        // Handle remaining bits
        if num_bits > 0 {
            let idx = ((bits << (5 - num_bits)) & 0x1f) as usize;
            result.push(BASE32_ALPHABET[idx] as char);
        }

        result
    }

    fn from_base32(s: &[u8]) -> Option<Self> {
        let mut bytes = [0u8; 32];
        let mut bits: u64 = 0;
        let mut num_bits = 0;
        let mut byte_idx = 0;

        for &c in s {
            // Convert character to 5-bit value
            let val = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a', // Case insensitive
                b'2'..=b'7' => c - b'2' + 26,
                _ => return None,
            };

            bits = (bits << 5) | (val as u64);
            num_bits += 5;

            if num_bits >= 8 {
                num_bits -= 8;
                if byte_idx >= 32 {
                    return None;
                }
                bytes[byte_idx] = ((bits >> num_bits) & 0xff) as u8;
                byte_idx += 1;
            }
        }

        if byte_idx != 32 {
            return None;
        }

        Some(Merkle(bytes))
    }
}

impl From<blake3::Hash> for Merkle {
    fn from(h: blake3::Hash) -> Self {
        Merkle(h.into())
    }
}

impl From<[u8; 32]> for Merkle {
    fn from(bytes: [u8; 32]) -> Self {
        Merkle(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_of() {
        let h1 = Hash::of(b"hello");
        let h2 = Hash::of(b"hello");
        let h3 = Hash::of(b"world");

        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert!(!h1.is_zero());
        assert!(Hash::ZERO.is_zero());
    }

    #[test]
    fn test_hash_zero_and_none() {
        assert!(Hash::ZERO.is_zero());
        assert!(Hash::NONE.is_none());
        assert_eq!(Hash::ZERO, Hash::NONE);
    }

    #[test]
    fn test_hash_hex_roundtrip() {
        let original = Hash::of(b"test data");
        let hex = original.to_hex();
        let parsed = Hash::from_hex(&hex).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_hex_invalid() {
        assert!(Hash::from_hex("").is_none());
        assert!(Hash::from_hex("abc").is_none());
        assert!(Hash::from_hex("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn test_hash_base32_roundtrip() {
        let original = Hash::of(b"test data for base32");
        let base32 = original.to_base32();
        let parsed = Hash::from_base32(base32.as_bytes()).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_base32_case_insensitive() {
        let original = Hash::of(b"case test");
        let base32_upper = original.to_base32();
        let base32_lower = base32_upper.to_lowercase();

        let parsed_upper = Hash::from_base32(base32_upper.as_bytes()).unwrap();
        let parsed_lower = Hash::from_base32(base32_lower.as_bytes()).unwrap();

        assert_eq!(parsed_upper, parsed_lower);
        assert_eq!(original, parsed_lower);
    }

    #[test]
    fn test_merkle_incremental() {
        let state0 = Merkle::initial();
        let change1 = Hash::of(b"change 1");
        let change2 = Hash::of(b"change 2");

        let state1 = state0.next(&change1);
        let state2 = state1.next(&change2);

        // States should all be different
        assert_ne!(state0, state1);
        assert_ne!(state1, state2);
        assert_ne!(state0, state2);

        // Same sequence produces same result
        let state1_again = state0.next(&change1);
        assert_eq!(state1, state1_again);
    }

    #[test]
    fn test_merkle_order_matters() {
        let state0 = Merkle::initial();
        let change1 = Hash::of(b"change 1");
        let change2 = Hash::of(b"change 2");

        let state_12 = state0.next(&change1).next(&change2);
        let state_21 = state0.next(&change2).next(&change1);

        // Different order produces different result
        assert_ne!(state_12, state_21);
    }

    #[test]
    fn test_hasher() {
        let mut hasher = Hasher::new();
        hasher.update(b"hello ");
        hasher.update(b"world");
        let h1 = hasher.finalize();

        let h2 = Hash::of(b"hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hasher_default() {
        let hasher = Hasher::default();
        let h1 = hasher.finalize();
        let h2 = Merkle::initial();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_json_roundtrip() {
        let original = Hash::of(b"json test");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_hash_bincode_roundtrip() {
        let original = Hash::of(b"bincode test");
        let bytes = bincode::serialize(&original).unwrap();
        let parsed: Hash = bincode::deserialize(&bytes).unwrap();
        assert_eq!(original, parsed);
        // Binary format should be exactly 32 bytes (fixed array)
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn test_hash_debug_display() {
        let h = Hash::of(b"display test");
        let debug = format!("{:?}", h);
        let display = format!("{}", h);

        // Debug should be short (truncated hex)
        assert!(debug.starts_with("Hash("));
        assert!(debug.len() < 30);

        // Display should be base32 (52 chars)
        assert_eq!(display.len(), 52);
        assert!(!display.contains("Hash"));
    }

    #[test]
    fn test_hash_type_alias() {
        // Verify Hash and Merkle are the same type
        let h: Hash = Hash::of(b"test");
        let m: Merkle = h;
        assert_eq!(h, m);

        // Can use Hash methods on Merkle and vice versa
        let state = m.next(&h);
        assert!(!state.is_none());
    }

    #[test]
    fn test_from_prefix() {
        let original = Hash::of(b"prefix test");
        let base32 = original.to_base32();

        // Should work with full string
        let from_full = Hash::from_prefix(&base32);
        assert!(from_full.is_some());

        // Prefix should work but produce different hash (padded)
        let prefix = &base32[..10];
        let from_prefix = Hash::from_prefix(prefix);
        assert!(from_prefix.is_some());
    }
}

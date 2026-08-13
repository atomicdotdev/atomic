//! Order-invariant set identity (`SetId`) for views and convergence.
//!
//! # Why this exists
//!
//! Atomic identifies a view's state with a running Merkle fold over its change
//! **sequence** (`state_n = H(state_{n-1} || change_n)`, see
//! [`Merkle::next`](super::Merkle::next)). That identity is *order-sensitive*:
//! two views holding the same **set** of changes in a different order — the
//! normal outcome of concurrent multi-agent writes and of `view split` /
//! reinsert — report different `Merkle` states even though their materialized
//! tree is byte-identical. That contradicts the property the content layer
//! already guarantees (independent changes commute).
//!
//! [`SetId`] is a second identity that lives **alongside** the order-sensitive
//! `Merkle`, never replacing it. It answers a different question — "do these
//! two views hold the same set of changes?" — with a single hash, regardless
//! of the order the changes were applied in.
//!
//! # Construction (AdHash — an additive homomorphic hash)
//!
//! Each change hash `h` is expanded to a 256-bit element
//! `e(h) = Blake3("atomic:setid:v1" || h)`, read as four little-endian `u64`
//! lanes. A `SetId` is the componentwise sum of the elements of its members,
//! with each lane added **mod 2^64** (independent wrapping lanes, no cross-lane
//! carry):
//!
//! ```text
//! SetId(S) = Σ_{h ∈ S} e(h)   (lanewise, mod 2^64)
//! ```
//!
//! Because integer addition is commutative and associative, the result is
//! independent of order (order-invariance), `combine` is componentwise
//! addition, and `remove` is componentwise subtraction — the exact inverse of
//! `add`. The empty set folds to all-zero lanes, which is [`SetId::ZERO`].
//!
//! ## Simplification guard: this is a convergence identity, not a trust boundary
//!
//! The intent recommended a wide LtHash-style lattice accumulator (≈2 KiB) with
//! the public id derived as a Blake3 over its canonical bytes. This module
//! instead ships the **compact 32-byte additive digest (AdHash)** so that a
//! `SetId` is a single value that is simultaneously homomorphic (`add` /
//! `remove` / `combine`) *and* a fixed, base32-embeddable identity that mirrors
//! the [`Merkle`](super::Merkle) API. The tradeoff, stated explicitly:
//!
//! - AdHash is **materially stronger than the incumbent** `GeoStore::set_digest`
//!   (sort + FNV-64): collisions require a distinct sub-multiset whose sum
//!   matches across four independent 64-bit lanes seeded by a Blake3 random
//!   oracle (~256-bit generic hardness) versus a trivially-forgeable 64-bit FNV.
//! - AdHash is **weaker than a wide LtHash** against an adaptive adversary who
//!   can choose set members freely. `SetId` is therefore an **equivalence /
//!   convergence identity, not a trust boundary**. The authoritative
//!   change-hash list / view manifest remains the source of truth for what a
//!   view actually contains; `SetId` only accelerates "are these equal?"
//!   answers. If a wide LtHash is ever needed, it can replace the internal
//!   accumulator without changing the public `SetId` string contract.
//!
//! # The canonical domain (a cross-producer contract)
//!
//! For CLI/core, `atomic-storage`, and geodist to compute the **same** `SetId`
//! for the same project, they must fold **byte-for-byte the same domain**:
//!
//! - **Applyable artifact hashes only**: changes and tags.
//! - **Sidecars excluded**: `.provenance` and `.attest` files are audit nodes,
//!   not part of any view's applyable set, and must not be folded in.
//! - Each member is fed in as its 32-byte [`Merkle`](super::Merkle) (the same
//!   value the base32 change hash decodes to).
//!
//! A domain mismatch silently breaks convergence equality, so the domain is a
//! documented contract rather than an implementation detail. `SetId` itself is
//! agnostic to *which* hashes it is given — enforcing the domain is the caller's
//! responsibility.
//!
//! # String form
//!
//! The base32 alphabet is RFC 4648 (`A–Z`, `2–7`) — it **never contains `-`**,
//! so a `SetId` can be embedded unambiguously in geodist object keys such as
//! `snapshots/pristine.redb@{ts}-{setdigest}`.

use super::Base32;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Domain-separation prefix for expanding a change hash into a `SetId` element.
/// Changing this string changes every `SetId` and breaks cross-producer
/// convergence — it is part of the wire/identity contract.
const SETID_DOMAIN: &[u8] = b"atomic:setid:v1";

/// An order-invariant identity for a **set** of change hashes.
///
/// `SetId` is a 32-byte additive homomorphic digest: the componentwise sum
/// (four independent little-endian `u64` lanes, mod 2^64) of a domain-separated
/// expansion of each member hash. See the [module docs](self) for the
/// construction, the convergence-identity tradeoff, and the canonical domain
/// contract.
///
/// It mirrors the [`Merkle`](super::Merkle) API surface (`ZERO`, `from_bytes`,
/// `as_bytes`, `to_base32`/`from_base32`, `to_hex`/`from_hex`, serde, `Display`,
/// `Debug`) so it can be used interchangeably in identity-carrying code.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SetId(pub [u8; 32]);

/// Read a 32-byte buffer as four little-endian `u64` lanes.
#[inline]
fn lanes_from_bytes(bytes: &[u8; 32]) -> [u64; 4] {
    let mut lanes = [0u64; 4];
    for (i, lane) in lanes.iter_mut().enumerate() {
        let mut chunk = [0u8; 8];
        chunk.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        *lane = u64::from_le_bytes(chunk);
    }
    lanes
}

/// Write four little-endian `u64` lanes back into a 32-byte buffer.
#[inline]
fn bytes_from_lanes(lanes: [u64; 4]) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, lane) in lanes.iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&lane.to_le_bytes());
    }
    bytes
}

impl SetId {
    /// The identity element: the `SetId` of the empty set (all-zero lanes).
    pub const ZERO: SetId = SetId([0u8; 32]);

    /// Digest size in bytes.
    pub const SIZE: usize = 32;

    /// Wrap raw bytes as a `SetId`.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        SetId(bytes)
    }

    /// Borrow the underlying bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Whether this is the empty-set identity ([`SetId::ZERO`]).
    #[inline]
    pub fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }

    /// Expand a change hash into its four-lane `SetId` element via a
    /// domain-separated Blake3 hash.
    #[inline]
    fn element_lanes(change: &super::Merkle) -> [u64; 4] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(SETID_DOMAIN);
        hasher.update(change.as_bytes());
        let out: [u8; 32] = hasher.finalize().into();
        lanes_from_bytes(&out)
    }

    /// Return the `SetId` with `change` added to the set.
    ///
    /// Adding is commutative and associative, so folding a set in any order
    /// yields the same `SetId`. Adding the same hash twice models a multiset
    /// (two members); callers that want set semantics must deduplicate first.
    #[must_use]
    pub fn add(&self, change: &super::Merkle) -> Self {
        let e = Self::element_lanes(change);
        let mut lanes = lanes_from_bytes(&self.0);
        for (l, r) in lanes.iter_mut().zip(e.iter()) {
            *l = l.wrapping_add(*r);
        }
        SetId(bytes_from_lanes(lanes))
    }

    /// Return the `SetId` with `change` removed from the set.
    ///
    /// `remove` is the exact inverse of [`add`](Self::add):
    /// `s.add(h).remove(h) == s` for every `s` and `h`.
    #[must_use]
    pub fn remove(&self, change: &super::Merkle) -> Self {
        let e = Self::element_lanes(change);
        let mut lanes = lanes_from_bytes(&self.0);
        for (l, r) in lanes.iter_mut().zip(e.iter()) {
            *l = l.wrapping_sub(*r);
        }
        SetId(bytes_from_lanes(lanes))
    }

    /// Combine two set identities into the identity of their multiset union.
    ///
    /// `combine` is componentwise addition, so
    /// `SetId::of(a).combine(&SetId::of(b)) == SetId::of(a ∪ b)` (as multisets).
    #[must_use]
    pub fn combine(&self, other: &SetId) -> Self {
        let a = lanes_from_bytes(&self.0);
        let b = lanes_from_bytes(&other.0);
        let mut lanes = [0u64; 4];
        for i in 0..4 {
            lanes[i] = a[i].wrapping_add(b[i]);
        }
        SetId(bytes_from_lanes(lanes))
    }

    /// Fold an iterator of change hashes into a `SetId`, starting from
    /// [`SetId::ZERO`]. Order does not affect the result.
    ///
    /// The caller is responsible for supplying the canonical domain (applyable
    /// change and tag hashes, sidecars excluded) — see the [module docs](self).
    pub fn of<'a, I>(changes: I) -> Self
    where
        I: IntoIterator<Item = &'a super::Merkle>,
    {
        let mut acc = Self::ZERO;
        for change in changes {
            acc = acc.add(change);
        }
        acc
    }

    /// Convert to a lowercase hex string (64 characters).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in &self.0 {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    /// Parse from a 64-character lowercase/uppercase hex string.
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex_str = std::str::from_utf8(chunk).ok()?;
            bytes[i] = u8::from_str_radix(hex_str, 16).ok()?;
        }
        Some(SetId(bytes))
    }
}

impl fmt::Debug for SetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SetId({})", &self.to_hex()[..12])
    }
}

impl fmt::Display for SetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_base32())
    }
}

impl Serialize for SetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_base32())
        } else {
            serializer.serialize_newtype_struct("SetId", &self.0)
        }
    }
}

impl<'de> Deserialize<'de> for SetId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            SetId::from_base32(s.as_bytes())
                .ok_or_else(|| serde::de::Error::custom("invalid base32 SetId"))
        } else {
            struct SetIdVisitor;

            impl<'de> serde::de::Visitor<'de> for SetIdVisitor {
                type Value = SetId;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("a 32-byte SetId")
                }

                fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    let bytes = <[u8; 32]>::deserialize(deserializer)?;
                    Ok(SetId(bytes))
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
                    Ok(SetId(bytes))
                }
            }

            deserializer.deserialize_newtype_struct("SetId", SetIdVisitor)
        }
    }
}

// Base32 alphabet (RFC 4648, no padding) — identical to `Merkle`'s, and
// crucially free of `-` so a `SetId` embeds cleanly in geodist object keys.
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl Base32 for SetId {
    fn to_base32(&self) -> String {
        // 32 bytes -> 52 base32 characters (ceil(32*8/5)).
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

        Some(SetId(bytes))
    }
}

impl From<[u8; 32]> for SetId {
    fn from(bytes: [u8; 32]) -> Self {
        SetId(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Merkle;

    /// A cheap deterministic pseudo-random change hash generator so the
    /// property tests don't need an external rng dependency.
    fn change(seed: u64) -> Merkle {
        Merkle::of(format!("change-{seed}").as_bytes())
    }

    #[test]
    fn empty_set_is_zero() {
        // (c) The empty set maps to SetId::ZERO.
        let empty: Vec<Merkle> = Vec::new();
        assert_eq!(SetId::of(empty.iter()), SetId::ZERO);
        assert!(SetId::ZERO.is_zero());
    }

    #[test]
    fn permutation_invariance() {
        // (a) Any permutation of the same set yields an identical SetId.
        let hashes: Vec<Merkle> = (0..64).map(change).collect();

        let forward = SetId::of(hashes.iter());
        let backward = SetId::of(hashes.iter().rev());

        // A few deterministic shuffles (swap pairs, rotate) — no rng needed.
        let mut rotated = hashes.clone();
        rotated.rotate_left(17);
        let rotated_id = SetId::of(rotated.iter());

        let mut swapped = hashes.clone();
        swapped.swap(3, 61);
        swapped.swap(0, 40);
        let swapped_id = SetId::of(swapped.iter());

        assert_eq!(forward, backward);
        assert_eq!(forward, rotated_id);
        assert_eq!(forward, swapped_id);
    }

    #[test]
    fn remove_inverts_add() {
        // (b) remove(add(s, h), h) == s for arbitrary s and h.
        let base: Vec<Merkle> = (100..140).map(change).collect();
        let s = SetId::of(base.iter());

        for seed in [0u64, 1, 7, 999, u64::MAX] {
            let h = change(seed);
            assert_eq!(s.add(&h).remove(&h), s);
            // Order of the two operations does not matter either.
            assert_eq!(s.remove(&h).add(&h), s);
        }
    }

    #[test]
    fn combine_equals_union() {
        // combine is the identity of the multiset union.
        let a: Vec<Merkle> = (0..30).map(change).collect();
        let b: Vec<Merkle> = (30..70).map(change).collect();

        let union: Vec<Merkle> = a.iter().chain(b.iter()).copied().collect();

        let combined = SetId::of(a.iter()).combine(&SetId::of(b.iter()));
        assert_eq!(combined, SetId::of(union.iter()));
    }

    #[test]
    fn different_sets_differ() {
        // (d) Distinct sets yield distinct SetIds across randomized inputs.
        use std::collections::HashSet;

        let mut ids: HashSet<[u8; 32]> = HashSet::new();
        // 500 distinct sets: set k = {change(0..k+1)} plus a unique marker.
        for k in 0..500u64 {
            let mut members: Vec<Merkle> = (0..=k).map(change).collect();
            members.push(Merkle::of(format!("marker-{k}").as_bytes()));
            let id = SetId::of(members.iter());
            assert!(
                ids.insert(id.0),
                "collision at k={k}: SetId {} already seen",
                id
            );
        }

        // Two single-element sets that differ only by their one member.
        assert_ne!(SetId::ZERO.add(&change(1)), SetId::ZERO.add(&change(2)));
    }

    #[test]
    fn adding_twice_is_a_multiset() {
        // A member added twice is not the same as added once (multiset), and
        // removing once returns to the single-member identity.
        let h = change(42);
        let once = SetId::ZERO.add(&h);
        let twice = once.add(&h);
        assert_ne!(once, twice);
        assert_eq!(twice.remove(&h), once);
    }

    #[test]
    fn base32_roundtrip_and_no_dash() {
        let id = SetId::of((0..10).map(change).collect::<Vec<_>>().iter());
        let s = id.to_base32();
        assert_eq!(s.len(), 52);
        assert!(!s.contains('-'), "SetId string must never contain '-'");
        let parsed = SetId::from_base32(s.as_bytes()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn base32_is_case_insensitive() {
        let id = SetId::ZERO.add(&change(5));
        let upper = id.to_base32();
        let lower = upper.to_lowercase();
        assert_eq!(
            SetId::from_base32(upper.as_bytes()).unwrap(),
            SetId::from_base32(lower.as_bytes()).unwrap()
        );
    }

    #[test]
    fn hex_roundtrip() {
        let id = SetId::of((20..25).map(change).collect::<Vec<_>>().iter());
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(SetId::from_hex(&hex).unwrap(), id);
    }

    #[test]
    fn json_and_postcard_roundtrip() {
        let id = SetId::of((0..8).map(change).collect::<Vec<_>>().iter());

        let json = serde_json::to_string(&id).unwrap();
        let from_json: SetId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, from_json);

        let bytes = postcard::to_allocvec(&id).unwrap();
        let from_postcard: SetId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, from_postcard);
        assert_eq!(bytes.len(), 32, "binary SetId is exactly 32 bytes");
    }

    #[test]
    fn debug_and_display() {
        let id = SetId::ZERO.add(&change(1));
        let debug = format!("{id:?}");
        let display = format!("{id}");
        assert!(debug.starts_with("SetId("));
        assert!(debug.len() < 24);
        assert_eq!(display.len(), 52);
        assert!(!display.contains("SetId"));
    }
}

//! Hash index type and compact position for V3 serialization.
//!
//! [`HashIndex`] is a `u16` reference into the hash deduplication table,
//! replacing full 32-byte hashes throughout serialized change files.
//! [`CompactPosition`] pairs a `HashIndex` with a `u32` byte offset.

use serde::{Deserialize, Serialize};
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// HashIndex — a u16 reference into the hash deduplication table
// ═══════════════════════════════════════════════════════════════════════

/// A reference to a hash in the deduplication table.
///
/// Instead of storing full 32-byte hashes throughout the change, we store
/// a compact `u16` index that points into a table of unique hashes at
/// the top of the file.
///
/// # Special Values
///
/// - **Index 0**: Always the change's own hash (self-reference).
/// - **Index 0xFFFF (`NONE`)**: Sentinel for "no hash" — used for root
///   positions and other cases where no hash is needed. This is equivalent
///   to `Option::<Hash>::None` in V1/V2.
///
/// # Capacity
///
/// With `u16` indices, the table supports up to 65,534 unique hashes
/// (0x0000 through 0xFFFE). Index 0xFFFF is reserved. In practice,
/// most changes reference fewer than 100 unique hashes.
///
/// # Serialization
///
/// When serialized with postcard, a `HashIndex` value of 0 takes only 1 byte
/// (varint encoding), values up to 127 take 1 byte, and the maximum value
/// takes 3 bytes. This is a massive improvement over the 33-byte
/// `Option<Hash>` in V1/V2's bincode encoding.
pub type HashIndex = u16;

/// Sentinel value meaning "no hash" (equivalent to `None` in `Option<Hash>`).
///
/// Used for root positions and other cases where a position doesn't
/// reference any specific change. This is the `u16` equivalent of
/// `Option::<Hash>::None`.
///
/// This value must never appear as a valid index in the hash dedup table.
pub const HASH_INDEX_NONE: HashIndex = 0xFFFF;

/// Index reserved for the change's own hash.
///
/// By convention, index 0 in the hash dedup table always holds the hash
/// of the change being serialized. This means all self-referencing
/// positions use index 0, which encodes to a single byte in postcard.
pub const HASH_INDEX_SELF: HashIndex = 0;

/// Returns `true` if this index represents "no hash" (the root/none sentinel).
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::{HASH_INDEX_NONE, HASH_INDEX_SELF, is_none_index};
///
/// assert!(is_none_index(HASH_INDEX_NONE));
/// assert!(!is_none_index(HASH_INDEX_SELF));
/// assert!(!is_none_index(42));
/// ```
#[inline]
pub fn is_none_index(index: HashIndex) -> bool {
    index == HASH_INDEX_NONE
}

// ═══════════════════════════════════════════════════════════════════════
// CompactPosition — a position using HashIndex instead of full hashes
// ═══════════════════════════════════════════════════════════════════════

/// A position in the repository graph using hash table indices.
///
/// This is the V3 equivalent of `Position<Option<Hash>>` from V1/V2.
/// Instead of storing a full 32-byte hash, it stores a `u16` index into
/// the hash deduplication table.
///
/// # Size Comparison
///
/// | Format | Hash field | Position field | Total |
/// |--------|-----------|----------------|-------|
/// | V1/V2 (bincode) | 33 bytes (`Option<Hash>`) | 8 bytes (`u64`) | 41 bytes |
/// | V3 (postcard) | 1-3 bytes (`HashIndex` varint) | 1-5 bytes (`u32` varint) | 2-8 bytes |
///
/// For an initial record where every position references the same change
/// (index 0), the `change` field is always 0, which postcard encodes as
/// a single byte. Combined with small position offsets, most positions
/// take only 2-3 bytes.
///
/// # Serialization
///
/// This struct derives `serde::Serialize` and `serde::Deserialize` and is
/// designed to be serialized with the `postcard` crate for maximum compactness.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::{CompactPosition, HASH_INDEX_SELF, HASH_INDEX_NONE};
///
/// // A position in the change's own content at byte offset 42
/// let pos = CompactPosition::new(HASH_INDEX_SELF, 42);
/// assert_eq!(pos.change, 0);
/// assert_eq!(pos.pos, 42);
/// assert!(!pos.is_root());
///
/// // A root position (no associated change)
/// let root = CompactPosition::root(100);
/// assert!(root.is_root());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CompactPosition {
    /// Index into the hash dedup table identifying which change this
    /// position belongs to.
    ///
    /// - `0` = this change itself (see [`HASH_INDEX_SELF`])
    /// - `0xFFFF` = no change / root (see [`HASH_INDEX_NONE`])
    /// - `1..=0xFFFE` = a dependency change
    pub change: HashIndex,

    /// Byte offset within the change's content blob.
    ///
    /// This is a `u32` instead of V1/V2's `u64` because individual changes
    /// are limited to 4 GB of content. For repository-wide positions that
    /// exceed this, the graph layer uses `u64` internally — the `u32` here
    /// is only for the serialized change file format.
    pub pos: u32,
}

impl CompactPosition {
    /// Create a new position referencing a specific change and byte offset.
    ///
    /// # Arguments
    ///
    /// * `change` - Index into the hash dedup table
    /// * `pos` - Byte offset within the change's content
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{CompactPosition, HASH_INDEX_SELF};
    ///
    /// let pos = CompactPosition::new(HASH_INDEX_SELF, 100);
    /// assert_eq!(pos.change, HASH_INDEX_SELF);
    /// assert_eq!(pos.pos, 100);
    /// ```
    #[inline]
    pub const fn new(change: HashIndex, pos: u32) -> Self {
        Self { change, pos }
    }

    /// Create a root position (no associated change) at the given offset.
    ///
    /// Root positions use [`HASH_INDEX_NONE`] as their change index.
    /// These represent positions in the virtual root of the repository graph.
    ///
    /// # Arguments
    ///
    /// * `pos` - Byte offset
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{CompactPosition, HASH_INDEX_NONE};
    ///
    /// let root = CompactPosition::root(0);
    /// assert_eq!(root.change, HASH_INDEX_NONE);
    /// assert!(root.is_root());
    /// ```
    #[inline]
    pub const fn root(pos: u32) -> Self {
        Self {
            change: HASH_INDEX_NONE,
            pos,
        }
    }

    /// Create a self-referencing position (references this change's own content).
    ///
    /// Self-referencing positions use [`HASH_INDEX_SELF`] (index 0) as their
    /// change index. This is the most common case during recording — all new
    /// content positions reference the change being created.
    ///
    /// # Arguments
    ///
    /// * `pos` - Byte offset within this change's content blob
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{CompactPosition, HASH_INDEX_SELF};
    ///
    /// let pos = CompactPosition::self_ref(42);
    /// assert_eq!(pos.change, HASH_INDEX_SELF);
    /// assert!(!pos.is_root());
    /// ```
    #[inline]
    pub const fn self_ref(pos: u32) -> Self {
        Self {
            change: HASH_INDEX_SELF,
            pos,
        }
    }

    /// Returns `true` if this is a root position (no associated change).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::CompactPosition;
    ///
    /// assert!(CompactPosition::root(0).is_root());
    /// assert!(!CompactPosition::self_ref(0).is_root());
    /// ```
    #[inline]
    pub const fn is_root(&self) -> bool {
        self.change == HASH_INDEX_NONE
    }

    /// Returns `true` if this position references the change's own content.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::CompactPosition;
    ///
    /// assert!(CompactPosition::self_ref(0).is_self_ref());
    /// assert!(!CompactPosition::root(0).is_self_ref());
    /// ```
    #[inline]
    pub const fn is_self_ref(&self) -> bool {
        self.change == HASH_INDEX_SELF
    }
}

impl fmt::Display for CompactPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            write!(f, "ROOT:{}", self.pos)
        } else if self.is_self_ref() {
            write!(f, "SELF:{}", self.pos)
        } else {
            write!(f, "#{}:{}", self.change, self.pos)
        }
    }
}

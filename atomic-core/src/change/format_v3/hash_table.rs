//! Hash deduplication table for the Change Format V3 serialization layer.
//!
//! This module implements the core space-saving optimization of V3: instead of
//! storing full 32-byte Blake3 hashes throughout the change file, we store them
//! **once** in a dedup table at the top of the file and reference them by `u16`
//! index everywhere else.
//!
//! # Problem
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash plus a
//! 1-byte `Option` discriminant (33 bytes total). A typical initial record of
//! 194K LOC produces ~500K position references, almost all referencing the same
//! change hash. That's `500K × 33 = ~16 MB` of redundant hash data.
//!
//! # Solution
//!
//! The `HashDedupTable` stores each unique hash exactly once, then all positions
//! reference hashes by their `u16` index. With postcard varint encoding, index 0
//! takes 1 byte, indices up to 127 take 1 byte, and the maximum index (0xFFFE)
//! takes 3 bytes. For that same 500K positions: `500K × 1 = ~500 KB`.
//!
//! # Table Structure
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  Index 0:  this change's own hash (always first)                │
//! │  Index 1:  first dependency hash                                │
//! │  Index 2:  second dependency hash                               │
//! │  ...                                                             │
//! │  Index N:  last unique hash referenced in the change             │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The table is serialized as a flat array of 32-byte hashes, uncompressed,
//! immediately after the 64-byte file header. It's uncompressed because:
//! 1. It's small (typically < 10 KB even for large changes)
//! 2. It's needed to interpret all subsequent sections
//! 3. It participates in the incremental blake3 hash computation
//!
//! # Special Index Values
//!
//! - **Index 0 (`HASH_INDEX_SELF`)**: Always the change's own hash. Since most
//!   positions in a new change reference the change itself, this is by far the
//!   most common index, and postcard encodes it as a single byte (0x00).
//! - **Index 0xFFFF (`HASH_INDEX_NONE`)**: Sentinel for "no hash" — used for
//!   root positions. This value never appears in the table itself.
//!
//! # Capacity
//!
//! The table supports up to 65,534 unique hashes (indices 0x0000 through 0xFFFE).
//! Index 0xFFFF is reserved as the "none" sentinel. In practice, most changes
//! reference fewer than 100 unique hashes.
//!
//! # Usage
//!
//! ## Building (during recording/writing)
//!
//! ```rust
//! use atomic_core::change::format_v3::HashDedupTable;
//!
//! let self_hash = [1u8; 32]; // the change's own hash
//! let dep_hash = [2u8; 32];  // a dependency hash
//!
//! let mut table = HashDedupTable::new(self_hash);
//!
//! // Register dependency hashes
//! let dep_index = table.insert(dep_hash).unwrap();
//! assert_eq!(dep_index, 1); // first dep gets index 1
//!
//! // Look up indices for positions
//! let self_index = table.lookup(&self_hash).unwrap();
//! assert_eq!(self_index, 0); // self hash is always index 0
//! ```
//!
//! ## Reading (during deserialization)
//!
//! ```rust
//! use atomic_core::change::format_v3::HashDedupTable;
//!
//! let self_hash = [1u8; 32];
//! let dep_hash = [2u8; 32];
//!
//! // Reconstruct from serialized hashes
//! let table = HashDedupTable::from_hashes(vec![self_hash, dep_hash]).unwrap();
//!
//! // Resolve indices back to hashes
//! assert_eq!(table.resolve(0).unwrap(), &self_hash);
//! assert_eq!(table.resolve(1).unwrap(), &dep_hash);
//! ```
//!
//! # Thread Safety
//!
//! `HashDedupTable` is NOT thread-safe. During parallel recording, each thread
//! should collect its referenced hashes into a local set, then merge them into
//! a single `HashDedupTable` on the main thread before serialization.

use super::error::{FormatError, FormatResult, MAX_HASH_TABLE_ENTRIES};
use super::types::{HashIndex, HASH_INDEX_NONE, HASH_INDEX_SELF};
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};

/// Size of a single hash entry in the table, in bytes.
const HASH_SIZE: usize = 32;

/// A deduplication table mapping 32-byte Blake3 hashes to compact `u16` indices.
///
/// This is the central data structure for the V3 format's space optimization.
/// It stores each unique hash exactly once and provides O(1) bidirectional
/// lookup between hashes and indices.
///
/// # Invariants
///
/// 1. Index 0 always holds the change's own hash (set at construction time).
/// 2. Indices are assigned sequentially starting from 0.
/// 3. No two indices map to the same hash (bijective mapping).
/// 4. The table never contains more than [`MAX_HASH_TABLE_ENTRIES`] entries.
/// 5. Index [`HASH_INDEX_NONE`] (0xFFFF) is never assigned to any hash.
///
/// # Performance
///
/// | Operation | Complexity | Notes |
/// |-----------|------------|-------|
/// | `insert` | O(1) amortized | HashMap insertion |
/// | `lookup` | O(1) | HashMap lookup |
/// | `resolve` | O(1) | Vec index |
/// | `contains` | O(1) | HashMap lookup |
/// | `serialize` | O(n) | n = number of entries |
/// | `deserialize` | O(n) | n = number of entries |
#[derive(Clone)]
pub struct HashDedupTable {
    /// Ordered list of hashes. The index in this vec IS the `HashIndex`.
    hashes: Vec<[u8; 32]>,

    /// Reverse lookup: hash → index. Used during writing to find the
    /// index for a given hash without scanning the `hashes` vec.
    index_map: HashMap<[u8; 32], HashIndex>,
}

impl HashDedupTable {
    /// Create a new dedup table with the change's own hash at index 0.
    ///
    /// The self hash is always the first entry in the table. All
    /// self-referencing positions in the change will use index 0.
    ///
    /// # Arguments
    ///
    /// * `self_hash` - The Blake3 hash of the change being written.
    ///   This is placed at index 0 ([`HASH_INDEX_SELF`]).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let hash = blake3::hash(b"my change").as_bytes().to_owned();
    /// let table = HashDedupTable::new(hash);
    /// assert_eq!(table.len(), 1);
    /// assert_eq!(table.resolve(0).unwrap(), &hash);
    /// ```
    pub fn new(self_hash: [u8; 32]) -> Self {
        let mut index_map = HashMap::with_capacity(16);
        index_map.insert(self_hash, HASH_INDEX_SELF);

        Self {
            hashes: vec![self_hash],
            index_map,
        }
    }

    /// Create an empty dedup table.
    ///
    /// This is only useful for testing or for cases where the self hash
    /// will be set later. In normal operation, use [`new`](Self::new).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let table = HashDedupTable::empty();
    /// assert_eq!(table.len(), 0);
    /// ```
    pub fn empty() -> Self {
        Self {
            hashes: Vec::new(),
            index_map: HashMap::new(),
        }
    }

    /// Reconstruct a dedup table from a list of hashes (read from a file).
    ///
    /// The first hash in the list is assumed to be the change's own hash
    /// (index 0). The order of hashes in the vector determines their indices.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Ordered list of 32-byte hashes as read from the file.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashTableFull`] if there are more than [`MAX_HASH_TABLE_ENTRIES`] hashes.
    /// - [`FormatError::InvalidHeader`] if duplicate hashes are found.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let h0 = [0u8; 32];
    /// let h1 = [1u8; 32];
    /// let table = HashDedupTable::from_hashes(vec![h0, h1]).unwrap();
    /// assert_eq!(table.len(), 2);
    /// assert_eq!(table.resolve(0).unwrap(), &h0);
    /// assert_eq!(table.resolve(1).unwrap(), &h1);
    /// ```
    pub fn from_hashes(hashes: Vec<[u8; 32]>) -> FormatResult<Self> {
        if hashes.len() > MAX_HASH_TABLE_ENTRIES {
            return Err(FormatError::HashTableFull);
        }

        let mut index_map = HashMap::with_capacity(hashes.len());
        for (i, hash) in hashes.iter().enumerate() {
            let index = i as HashIndex;
            if index_map.insert(*hash, index).is_some() {
                return Err(FormatError::InvalidHeader {
                    reason: format!("duplicate hash in dedup table at index {}", i),
                });
            }
        }

        Ok(Self { hashes, index_map })
    }

    /// Insert a hash into the table, returning its index.
    ///
    /// If the hash is already in the table, returns its existing index
    /// without inserting a duplicate (idempotent operation).
    ///
    /// # Arguments
    ///
    /// * `hash` - The 32-byte Blake3 hash to insert.
    ///
    /// # Returns
    ///
    /// The `HashIndex` for this hash (either newly assigned or existing).
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashTableFull`] if the table already has
    ///   [`MAX_HASH_TABLE_ENTRIES`] entries and this hash is new.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let self_hash = [0u8; 32];
    /// let mut table = HashDedupTable::new(self_hash);
    ///
    /// let dep = [1u8; 32];
    /// let idx1 = table.insert(dep).unwrap();
    /// let idx2 = table.insert(dep).unwrap(); // idempotent
    /// assert_eq!(idx1, idx2);
    /// assert_eq!(table.len(), 2); // self + 1 dep
    /// ```
    pub fn insert(&mut self, hash: [u8; 32]) -> FormatResult<HashIndex> {
        if let Some(&existing) = self.index_map.get(&hash) {
            return Ok(existing);
        }

        if self.hashes.len() >= MAX_HASH_TABLE_ENTRIES {
            return Err(FormatError::HashTableFull);
        }

        let index = self.hashes.len() as HashIndex;
        self.hashes.push(hash);
        self.index_map.insert(hash, index);
        Ok(index)
    }

    /// Look up the index for a given hash.
    ///
    /// Returns `None` if the hash is not in the table. This is an O(1)
    /// operation using the internal `HashMap`.
    ///
    /// # Arguments
    ///
    /// * `hash` - The 32-byte hash to look up.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let self_hash = [0u8; 32];
    /// let table = HashDedupTable::new(self_hash);
    ///
    /// assert_eq!(table.lookup(&self_hash), Some(0));
    /// assert_eq!(table.lookup(&[99u8; 32]), None);
    /// ```
    pub fn lookup(&self, hash: &[u8; 32]) -> Option<HashIndex> {
        self.index_map.get(hash).copied()
    }

    /// Look up the index for a hash, returning an error if not found.
    ///
    /// This is a convenience method that wraps [`lookup`](Self::lookup)
    /// and converts `None` to [`FormatError::HashNotFound`].
    ///
    /// # Arguments
    ///
    /// * `hash` - The 32-byte hash to look up.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashNotFound`] if the hash isn't in the table.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let self_hash = [0u8; 32];
    /// let table = HashDedupTable::new(self_hash);
    ///
    /// assert!(table.require(&self_hash).is_ok());
    /// assert!(table.require(&[99u8; 32]).is_err());
    /// ```
    pub fn require(&self, hash: &[u8; 32]) -> FormatResult<HashIndex> {
        self.lookup(hash).ok_or_else(|| FormatError::HashNotFound {
            hash: data_encoding::BASE32_NOPAD.encode(&hash[..8]),
        })
    }

    /// Resolve an index back to its 32-byte hash.
    ///
    /// Returns `None` if the index is out of bounds or is the "none"
    /// sentinel ([`HASH_INDEX_NONE`]).
    ///
    /// # Arguments
    ///
    /// * `index` - The `HashIndex` to resolve.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{HashDedupTable, HASH_INDEX_NONE};
    ///
    /// let hash = [42u8; 32];
    /// let table = HashDedupTable::new(hash);
    ///
    /// assert_eq!(table.resolve(0), Some(&hash));
    /// assert_eq!(table.resolve(1), None);         // out of bounds
    /// assert_eq!(table.resolve(HASH_INDEX_NONE), None); // sentinel
    /// ```
    pub fn resolve(&self, index: HashIndex) -> Option<&[u8; 32]> {
        if index == HASH_INDEX_NONE {
            return None;
        }
        self.hashes.get(index as usize)
    }

    /// Resolve an index back to its hash, returning an error if not found.
    ///
    /// This is a convenience method that wraps [`resolve`](Self::resolve)
    /// and converts `None` to [`FormatError::HashIndexOutOfBounds`].
    ///
    /// Note: This does NOT error on [`HASH_INDEX_NONE`] — that's a valid
    /// sentinel value meaning "no hash." Instead, it returns `None` wrapped
    /// in `Ok`. Use [`resolve_required`](Self::resolve_required) if the
    /// index must map to an actual hash.
    ///
    /// # Arguments
    ///
    /// * `index` - The `HashIndex` to resolve.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashIndexOutOfBounds`] if the index exceeds the table size
    ///   (and is not `HASH_INDEX_NONE`).
    pub fn resolve_or_none(&self, index: HashIndex) -> FormatResult<Option<&[u8; 32]>> {
        if index == HASH_INDEX_NONE {
            return Ok(None);
        }
        if (index as usize) >= self.hashes.len() {
            return Err(FormatError::HashIndexOutOfBounds {
                index,
                table_size: self.hashes.len() as u16,
            });
        }
        Ok(Some(&self.hashes[index as usize]))
    }

    /// Resolve an index to a hash, erroring if it's `NONE` or out of bounds.
    ///
    /// Unlike [`resolve_or_none`](Self::resolve_or_none), this method treats
    /// [`HASH_INDEX_NONE`] as an error. Use this when you need an actual hash
    /// (e.g., resolving a dependency index).
    ///
    /// # Arguments
    ///
    /// * `index` - The `HashIndex` to resolve.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashIndexOutOfBounds`] if the index is `HASH_INDEX_NONE`
    ///   or exceeds the table size.
    pub fn resolve_required(&self, index: HashIndex) -> FormatResult<&[u8; 32]> {
        if index == HASH_INDEX_NONE || (index as usize) >= self.hashes.len() {
            return Err(FormatError::HashIndexOutOfBounds {
                index,
                table_size: self.hashes.len() as u16,
            });
        }
        Ok(&self.hashes[index as usize])
    }

    /// Returns `true` if the table contains the given hash.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let hash = [1u8; 32];
    /// let table = HashDedupTable::new(hash);
    /// assert!(table.contains(&hash));
    /// assert!(!table.contains(&[2u8; 32]));
    /// ```
    #[inline]
    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.index_map.contains_key(hash)
    }

    /// Returns the number of entries in the table.
    ///
    /// This is always >= 1 if the table was created with [`new`](Self::new)
    /// (the self hash is always present).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let table = HashDedupTable::new([0u8; 32]);
    /// assert_eq!(table.len(), 1);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Returns `true` if the table is empty.
    ///
    /// A table created with [`new`](Self::new) is never empty (it always
    /// has the self hash). Only tables created with [`empty`](Self::empty)
    /// can be empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Returns the self hash (index 0), if the table is non-empty.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let hash = [42u8; 32];
    /// let table = HashDedupTable::new(hash);
    /// assert_eq!(table.self_hash(), Some(&hash));
    /// ```
    pub fn self_hash(&self) -> Option<&[u8; 32]> {
        self.hashes.first()
    }

    /// Returns an iterator over all (index, hash) pairs in table order.
    ///
    /// The iterator yields entries in index order (0, 1, 2, ...).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let h0 = [0u8; 32];
    /// let h1 = [1u8; 32];
    /// let mut table = HashDedupTable::new(h0);
    /// table.insert(h1).unwrap();
    ///
    /// let entries: Vec<_> = table.iter().collect();
    /// assert_eq!(entries.len(), 2);
    /// assert_eq!(entries[0], (0u16, &h0));
    /// assert_eq!(entries[1], (1u16, &h1));
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (HashIndex, &[u8; 32])> {
        self.hashes
            .iter()
            .enumerate()
            .map(|(i, h)| (i as HashIndex, h))
    }

    /// Returns a slice of all hashes in table order.
    ///
    /// This is the raw data that gets serialized to the file.
    pub fn hashes(&self) -> &[[u8; 32]] {
        &self.hashes
    }

    /// Returns the dependency hashes (all entries except index 0).
    ///
    /// Index 0 is the change's own hash, so dependencies start at index 1.
    /// Returns an empty slice if the table has 0 or 1 entries.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let self_hash = [0u8; 32];
    /// let dep1 = [1u8; 32];
    /// let dep2 = [2u8; 32];
    /// let mut table = HashDedupTable::new(self_hash);
    /// table.insert(dep1).unwrap();
    /// table.insert(dep2).unwrap();
    ///
    /// let deps = table.dependency_hashes();
    /// assert_eq!(deps.len(), 2);
    /// assert_eq!(deps[0], dep1);
    /// assert_eq!(deps[1], dep2);
    /// ```
    pub fn dependency_hashes(&self) -> &[[u8; 32]] {
        if self.hashes.len() <= 1 {
            &[]
        } else {
            &self.hashes[1..]
        }
    }

    /// Total serialized size of the hash table in bytes.
    ///
    /// This is `len() * 32` — each entry is a raw 32-byte hash with no
    /// framing or compression.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let table = HashDedupTable::new([0u8; 32]);
    /// assert_eq!(table.serialized_size(), 32);
    /// ```
    #[inline]
    pub fn serialized_size(&self) -> usize {
        self.hashes.len() * HASH_SIZE
    }

    /// Write the hash table to a writer.
    ///
    /// Writes `len() × 32` bytes: each hash in index order, raw (no compression).
    /// The caller is responsible for writing the file header (which includes
    /// `hash_table_entries`) before calling this.
    ///
    /// # Arguments
    ///
    /// * `writer` - The destination writer.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let table = HashDedupTable::new([0u8; 32]);
    /// let mut buf = Vec::new();
    /// table.write_to(&mut buf).unwrap();
    /// assert_eq!(buf.len(), 32);
    /// ```
    pub fn write_to<W: Write>(&self, writer: &mut W) -> FormatResult<()> {
        for hash in &self.hashes {
            writer.write_all(hash)?;
        }
        Ok(())
    }

    /// Read a hash table from a reader.
    ///
    /// Reads `count × 32` bytes and reconstructs the table with bidirectional
    /// lookup support. The first hash read becomes index 0 (the self hash).
    ///
    /// # Arguments
    ///
    /// * `reader` - The source reader, positioned at the start of the hash table.
    /// * `count` - Number of hash entries to read (from the file header's
    ///   `hash_table_entries` field).
    ///
    /// # Errors
    ///
    /// - I/O error if fewer than `count × 32` bytes are available.
    /// - [`FormatError::HashTableFull`] if `count` exceeds [`MAX_HASH_TABLE_ENTRIES`].
    /// - [`FormatError::InvalidHeader`] if duplicate hashes are found.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    /// use std::io::Cursor;
    ///
    /// // Write then read
    /// let original = HashDedupTable::new([42u8; 32]);
    /// let mut buf = Vec::new();
    /// original.write_to(&mut buf).unwrap();
    ///
    /// let mut cursor = Cursor::new(&buf);
    /// let loaded = HashDedupTable::read_from(&mut cursor, 1).unwrap();
    /// assert_eq!(loaded.len(), 1);
    /// assert_eq!(loaded.resolve(0).unwrap(), &[42u8; 32]);
    /// ```
    pub fn read_from<R: Read>(reader: &mut R, count: u32) -> FormatResult<Self> {
        if count as usize > MAX_HASH_TABLE_ENTRIES {
            return Err(FormatError::HashTableFull);
        }

        let mut hashes = Vec::with_capacity(count as usize);
        let mut buf = [0u8; HASH_SIZE];

        for _ in 0..count {
            reader.read_exact(&mut buf)?;
            hashes.push(buf);
        }

        Self::from_hashes(hashes)
    }

    /// Merge another set of hashes into this table.
    ///
    /// This is useful during parallel recording where each thread collects
    /// referenced hashes independently, then the main thread merges them
    /// into a single table.
    ///
    /// Hashes that already exist in the table are skipped (idempotent).
    ///
    /// # Arguments
    ///
    /// * `hashes` - Iterator of hashes to add.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashTableFull`] if merging would exceed capacity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let mut table = HashDedupTable::new([0u8; 32]);
    /// let new_hashes = vec![[1u8; 32], [2u8; 32], [0u8; 32]]; // last is dup
    /// table.merge(new_hashes.into_iter()).unwrap();
    /// assert_eq!(table.len(), 3); // self + 2 new (dup skipped)
    /// ```
    pub fn merge(&mut self, hashes: impl Iterator<Item = [u8; 32]>) -> FormatResult<()> {
        for hash in hashes {
            self.insert(hash)?;
        }
        Ok(())
    }

    /// Return statistics about the table for debugging and logging.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::HashDedupTable;
    ///
    /// let mut table = HashDedupTable::new([0u8; 32]);
    /// table.insert([1u8; 32]).unwrap();
    /// table.insert([2u8; 32]).unwrap();
    ///
    /// let stats = table.stats();
    /// assert_eq!(stats.entry_count, 3);
    /// assert_eq!(stats.dependency_count, 2);
    /// assert_eq!(stats.serialized_bytes, 96);
    /// ```
    pub fn stats(&self) -> HashDedupTableStats {
        let entry_count = self.hashes.len();
        HashDedupTableStats {
            entry_count,
            dependency_count: entry_count.saturating_sub(1),
            serialized_bytes: entry_count * HASH_SIZE,
        }
    }
}

impl fmt::Debug for HashDedupTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashDedupTable")
            .field("entries", &self.hashes.len())
            .field("serialized_bytes", &self.serialized_size())
            .finish()
    }
}

impl fmt::Display for HashDedupTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HashDedupTable({} entries, {} bytes)",
            self.hashes.len(),
            self.serialized_size()
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// HashDedupTableStats — statistics for logging / debugging
// ═══════════════════════════════════════════════════════════════════════

/// Statistics about a [`HashDedupTable`].
///
/// Useful for logging, progress reporting, and performance analysis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashDedupTableStats {
    /// Total number of entries in the table (including self hash).
    pub entry_count: usize,

    /// Number of dependency entries (entry_count - 1, or 0 if empty).
    pub dependency_count: usize,

    /// Total serialized size in bytes (entry_count × 32).
    pub serialized_bytes: usize,
}

impl fmt::Display for HashDedupTableStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} entries ({} deps), {} bytes on disk",
            self.entry_count, self.dependency_count, self.serialized_bytes
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a hash filled with a single byte value.
    fn make_hash(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    // ── Construction ───────────────────────────────────────────────

    #[test]
    fn test_new_has_self_hash_at_index_zero() {
        let hash = make_hash(0xAA);
        let table = HashDedupTable::new(hash);

        assert_eq!(table.len(), 1);
        assert!(!table.is_empty());
        assert_eq!(table.resolve(0), Some(&hash));
        assert_eq!(table.self_hash(), Some(&hash));
    }

    #[test]
    fn test_empty_table() {
        let table = HashDedupTable::empty();
        assert_eq!(table.len(), 0);
        assert!(table.is_empty());
        assert_eq!(table.self_hash(), None);
        assert_eq!(table.resolve(0), None);
    }

    #[test]
    fn test_from_hashes_single() {
        let h = make_hash(1);
        let table = HashDedupTable::from_hashes(vec![h]).unwrap();

        assert_eq!(table.len(), 1);
        assert_eq!(table.resolve(0), Some(&h));
        assert_eq!(table.self_hash(), Some(&h));
    }

    #[test]
    fn test_from_hashes_multiple() {
        let h0 = make_hash(0);
        let h1 = make_hash(1);
        let h2 = make_hash(2);

        let table = HashDedupTable::from_hashes(vec![h0, h1, h2]).unwrap();

        assert_eq!(table.len(), 3);
        assert_eq!(table.resolve(0), Some(&h0));
        assert_eq!(table.resolve(1), Some(&h1));
        assert_eq!(table.resolve(2), Some(&h2));
        assert_eq!(table.resolve(3), None);
    }

    #[test]
    fn test_from_hashes_empty() {
        let table = HashDedupTable::from_hashes(vec![]).unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn test_from_hashes_duplicate_error() {
        let h = make_hash(42);
        let result = HashDedupTable::from_hashes(vec![h, h]);
        assert!(result.is_err());
        if let Err(FormatError::InvalidHeader { reason }) = result {
            assert!(reason.contains("duplicate"), "reason: {}", reason);
        } else {
            panic!("expected InvalidHeader error with 'duplicate'");
        }
    }

    // ── Insert ─────────────────────────────────────────────────────

    #[test]
    fn test_insert_new_hash() {
        let mut table = HashDedupTable::new(make_hash(0));

        let idx = table.insert(make_hash(1)).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(table.len(), 2);

        let idx = table.insert(make_hash(2)).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn test_insert_idempotent() {
        let mut table = HashDedupTable::new(make_hash(0));

        let idx1 = table.insert(make_hash(1)).unwrap();
        let idx2 = table.insert(make_hash(1)).unwrap();
        assert_eq!(idx1, idx2);
        assert_eq!(table.len(), 2); // not 3
    }

    #[test]
    fn test_insert_self_hash_returns_zero() {
        let self_hash = make_hash(0);
        let mut table = HashDedupTable::new(self_hash);

        // Inserting the self hash again should return 0
        let idx = table.insert(self_hash).unwrap();
        assert_eq!(idx, HASH_INDEX_SELF);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn test_insert_sequential_indices() {
        let mut table = HashDedupTable::new(make_hash(0));

        for i in 1..=100u8 {
            let idx = table.insert(make_hash(i)).unwrap();
            assert_eq!(idx, i as HashIndex);
        }
        assert_eq!(table.len(), 101);
    }

    // ── Lookup ─────────────────────────────────────────────────────

    #[test]
    fn test_lookup_existing() {
        let h0 = make_hash(0);
        let h1 = make_hash(1);
        let mut table = HashDedupTable::new(h0);
        table.insert(h1).unwrap();

        assert_eq!(table.lookup(&h0), Some(0));
        assert_eq!(table.lookup(&h1), Some(1));
    }

    #[test]
    fn test_lookup_missing() {
        let table = HashDedupTable::new(make_hash(0));
        assert_eq!(table.lookup(&make_hash(99)), None);
    }

    #[test]
    fn test_require_existing() {
        let h = make_hash(42);
        let table = HashDedupTable::new(h);
        assert_eq!(table.require(&h).unwrap(), 0);
    }

    #[test]
    fn test_require_missing() {
        let table = HashDedupTable::new(make_hash(0));
        let result = table.require(&make_hash(99));
        assert!(result.is_err());
        assert!(matches!(result, Err(FormatError::HashNotFound { .. })));
    }

    // ── Resolve ────────────────────────────────────────────────────

    #[test]
    fn test_resolve_valid() {
        let h = make_hash(7);
        let table = HashDedupTable::new(h);
        assert_eq!(table.resolve(0), Some(&h));
    }

    #[test]
    fn test_resolve_out_of_bounds() {
        let table = HashDedupTable::new(make_hash(0));
        assert_eq!(table.resolve(1), None);
        assert_eq!(table.resolve(1000), None);
    }

    #[test]
    fn test_resolve_none_sentinel() {
        let table = HashDedupTable::new(make_hash(0));
        assert_eq!(table.resolve(HASH_INDEX_NONE), None);
    }

    #[test]
    fn test_resolve_or_none_valid() {
        let h = make_hash(5);
        let table = HashDedupTable::new(h);

        let result = table.resolve_or_none(0).unwrap();
        assert_eq!(result, Some(&h));
    }

    #[test]
    fn test_resolve_or_none_sentinel() {
        let table = HashDedupTable::new(make_hash(0));
        let result = table.resolve_or_none(HASH_INDEX_NONE).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_or_none_out_of_bounds_error() {
        let table = HashDedupTable::new(make_hash(0));
        let result = table.resolve_or_none(5);
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(FormatError::HashIndexOutOfBounds {
                index: 5,
                table_size: 1
            })
        ));
    }

    #[test]
    fn test_resolve_required_valid() {
        let h = make_hash(5);
        let table = HashDedupTable::new(h);
        assert_eq!(table.resolve_required(0).unwrap(), &h);
    }

    #[test]
    fn test_resolve_required_none_sentinel_errors() {
        let table = HashDedupTable::new(make_hash(0));
        let result = table.resolve_required(HASH_INDEX_NONE);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_required_out_of_bounds_errors() {
        let table = HashDedupTable::new(make_hash(0));
        let result = table.resolve_required(10);
        assert!(result.is_err());
    }

    // ── Contains ───────────────────────────────────────────────────

    #[test]
    fn test_contains() {
        let h0 = make_hash(0);
        let h1 = make_hash(1);
        let table = HashDedupTable::new(h0);

        assert!(table.contains(&h0));
        assert!(!table.contains(&h1));
    }

    // ── Iterator ───────────────────────────────────────────────────

    #[test]
    fn test_iter_order() {
        let h0 = make_hash(0);
        let h1 = make_hash(1);
        let h2 = make_hash(2);

        let mut table = HashDedupTable::new(h0);
        table.insert(h1).unwrap();
        table.insert(h2).unwrap();

        let entries: Vec<_> = table.iter().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (0, &h0));
        assert_eq!(entries[1], (1, &h1));
        assert_eq!(entries[2], (2, &h2));
    }

    #[test]
    fn test_iter_empty() {
        let table = HashDedupTable::empty();
        let entries: Vec<_> = table.iter().collect();
        assert!(entries.is_empty());
    }

    // ── Dependency hashes ──────────────────────────────────────────

    #[test]
    fn test_dependency_hashes_none() {
        let table = HashDedupTable::new(make_hash(0));
        assert!(table.dependency_hashes().is_empty());
    }

    #[test]
    fn test_dependency_hashes_some() {
        let mut table = HashDedupTable::new(make_hash(0));
        table.insert(make_hash(1)).unwrap();
        table.insert(make_hash(2)).unwrap();

        let deps = table.dependency_hashes();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], make_hash(1));
        assert_eq!(deps[1], make_hash(2));
    }

    #[test]
    fn test_dependency_hashes_empty_table() {
        let table = HashDedupTable::empty();
        assert!(table.dependency_hashes().is_empty());
    }

    // ── Serialized size ────────────────────────────────────────────

    #[test]
    fn test_serialized_size_one_entry() {
        let table = HashDedupTable::new(make_hash(0));
        assert_eq!(table.serialized_size(), 32);
    }

    #[test]
    fn test_serialized_size_many_entries() {
        let mut table = HashDedupTable::new(make_hash(0));
        for i in 1..10u8 {
            table.insert(make_hash(i)).unwrap();
        }
        assert_eq!(table.serialized_size(), 10 * 32);
    }

    #[test]
    fn test_serialized_size_empty() {
        let table = HashDedupTable::empty();
        assert_eq!(table.serialized_size(), 0);
    }

    // ── I/O roundtrip ──────────────────────────────────────────────

    #[test]
    fn test_write_read_roundtrip_single() {
        let h = make_hash(0xBB);
        let table = HashDedupTable::new(h);

        let mut buf = Vec::new();
        table.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), 32);

        let mut cursor = std::io::Cursor::new(&buf);
        let loaded = HashDedupTable::read_from(&mut cursor, 1).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.resolve(0), Some(&h));
    }

    #[test]
    fn test_write_read_roundtrip_multiple() {
        let hashes: Vec<[u8; 32]> = (0..10u8).map(make_hash).collect();

        let mut table = HashDedupTable::new(hashes[0]);
        for h in &hashes[1..] {
            table.insert(*h).unwrap();
        }

        let mut buf = Vec::new();
        table.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), 10 * 32);

        let mut cursor = std::io::Cursor::new(&buf);
        let loaded = HashDedupTable::read_from(&mut cursor, 10).unwrap();

        assert_eq!(loaded.len(), 10);
        for (i, h) in hashes.iter().enumerate() {
            assert_eq!(
                loaded.resolve(i as HashIndex),
                Some(h),
                "mismatch at index {}",
                i
            );
        }
    }

    #[test]
    fn test_write_read_roundtrip_empty() {
        let table = HashDedupTable::empty();

        let mut buf = Vec::new();
        table.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), 0);

        let mut cursor = std::io::Cursor::new(&buf);
        let loaded = HashDedupTable::read_from(&mut cursor, 0).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_read_truncated_data() {
        // Only 20 bytes — not enough for a single 32-byte hash
        let buf = [0u8; 20];
        let mut cursor = std::io::Cursor::new(&buf[..]);
        let result = HashDedupTable::read_from(&mut cursor, 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_too_many_entries() {
        let result = HashDedupTable::read_from(
            &mut std::io::Cursor::new(&[] as &[u8]),
            (MAX_HASH_TABLE_ENTRIES as u32) + 1,
        );
        assert!(matches!(result, Err(FormatError::HashTableFull)));
    }

    // ── Merge ──────────────────────────────────────────────────────

    #[test]
    fn test_merge_new_hashes() {
        let mut table = HashDedupTable::new(make_hash(0));
        table
            .merge(vec![make_hash(1), make_hash(2)].into_iter())
            .unwrap();

        assert_eq!(table.len(), 3);
        assert_eq!(table.lookup(&make_hash(1)), Some(1));
        assert_eq!(table.lookup(&make_hash(2)), Some(2));
    }

    #[test]
    fn test_merge_with_duplicates() {
        let mut table = HashDedupTable::new(make_hash(0));
        table.insert(make_hash(1)).unwrap();

        // Merge includes a duplicate (make_hash(1)) and a new one (make_hash(2))
        table
            .merge(vec![make_hash(1), make_hash(2), make_hash(0)].into_iter())
            .unwrap();

        assert_eq!(table.len(), 3); // 0, 1, 2 — no duplicates
    }

    #[test]
    fn test_merge_empty_iterator() {
        let mut table = HashDedupTable::new(make_hash(0));
        table.merge(std::iter::empty()).unwrap();
        assert_eq!(table.len(), 1);
    }

    // ── Stats ──────────────────────────────────────────────────────

    #[test]
    fn test_stats_empty() {
        let table = HashDedupTable::empty();
        let stats = table.stats();
        assert_eq!(stats.entry_count, 0);
        assert_eq!(stats.dependency_count, 0);
        assert_eq!(stats.serialized_bytes, 0);
    }

    #[test]
    fn test_stats_with_entries() {
        let mut table = HashDedupTable::new(make_hash(0));
        table.insert(make_hash(1)).unwrap();
        table.insert(make_hash(2)).unwrap();

        let stats = table.stats();
        assert_eq!(stats.entry_count, 3);
        assert_eq!(stats.dependency_count, 2);
        assert_eq!(stats.serialized_bytes, 96);
    }

    #[test]
    fn test_stats_display() {
        let table = HashDedupTable::new(make_hash(0));
        let stats = table.stats();
        let display = format!("{}", stats);
        assert!(display.contains("1 entries"));
        assert!(display.contains("0 deps"));
        assert!(display.contains("32 bytes"));
    }

    // ── Display / Debug ────────────────────────────────────────────

    #[test]
    fn test_debug_format() {
        let table = HashDedupTable::new(make_hash(0));
        let debug = format!("{:?}", table);
        assert!(debug.contains("HashDedupTable"));
        assert!(debug.contains("entries"));
    }

    #[test]
    fn test_display_format() {
        let mut table = HashDedupTable::new(make_hash(0));
        table.insert(make_hash(1)).unwrap();

        let display = format!("{}", table);
        assert!(display.contains("2 entries"));
        assert!(display.contains("64 bytes"));
    }

    // ── Clone ──────────────────────────────────────────────────────

    #[test]
    fn test_clone() {
        let mut original = HashDedupTable::new(make_hash(0));
        original.insert(make_hash(1)).unwrap();

        let cloned = original.clone();
        assert_eq!(cloned.len(), 2);
        assert_eq!(cloned.resolve(0), original.resolve(0));
        assert_eq!(cloned.resolve(1), original.resolve(1));
    }

    // ── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn test_hashes_method() {
        let h0 = make_hash(0);
        let h1 = make_hash(1);
        let mut table = HashDedupTable::new(h0);
        table.insert(h1).unwrap();

        let raw = table.hashes();
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0], h0);
        assert_eq!(raw[1], h1);
    }

    #[test]
    fn test_bidirectional_consistency() {
        // Every hash should roundtrip through lookup → resolve
        let mut table = HashDedupTable::new(make_hash(0));
        for i in 1..50u8 {
            table.insert(make_hash(i)).unwrap();
        }

        for i in 0..50u8 {
            let h = make_hash(i);
            let idx = table.lookup(&h).expect("lookup should succeed");
            let resolved = table.resolve(idx).expect("resolve should succeed");
            assert_eq!(resolved, &h, "bidirectional mismatch at {}", i);
        }
    }

    #[test]
    fn test_many_entries() {
        // Test with a realistic number of dependencies
        let mut table = HashDedupTable::new(make_hash(0));
        for i in 1..=255u8 {
            table.insert(make_hash(i)).unwrap();
        }

        assert_eq!(table.len(), 256);
        assert_eq!(table.dependency_hashes().len(), 255);

        // Verify all entries
        for i in 0..=255u8 {
            let h = make_hash(i);
            assert_eq!(table.lookup(&h), Some(i as HashIndex));
            assert_eq!(table.resolve(i as HashIndex), Some(&h));
        }
    }

    #[test]
    fn test_blake3_hashes() {
        // Test with real blake3 hashes (not synthetic fill patterns)
        let h0 = *blake3::hash(b"change 0").as_bytes();
        let h1 = *blake3::hash(b"change 1").as_bytes();
        let h2 = *blake3::hash(b"change 2").as_bytes();

        let mut table = HashDedupTable::new(h0);
        table.insert(h1).unwrap();
        table.insert(h2).unwrap();

        assert_eq!(table.lookup(&h0), Some(0));
        assert_eq!(table.lookup(&h1), Some(1));
        assert_eq!(table.lookup(&h2), Some(2));

        // Roundtrip through serialization
        let mut buf = Vec::new();
        table.write_to(&mut buf).unwrap();

        let mut cursor = std::io::Cursor::new(&buf);
        let loaded = HashDedupTable::read_from(&mut cursor, 3).unwrap();

        assert_eq!(loaded.resolve(0).unwrap(), &h0);
        assert_eq!(loaded.resolve(1).unwrap(), &h1);
        assert_eq!(loaded.resolve(2).unwrap(), &h2);
    }

    // ── Size savings calculation ───────────────────────────────────

    #[test]
    fn test_size_savings_estimate() {
        // Demonstrate the V2 → V3 size savings for positions
        //
        // V2: Option<Hash> = 1 (discriminant) + 32 (hash) = 33 bytes
        //     Position<Option<Hash>> = 33 + 8 (u64 pos) = 41 bytes
        //
        // V3: CompactPosition = varint(HashIndex) + varint(u32 pos)
        //     For index=0, pos=0: 1 + 1 = 2 bytes
        //     For index=0, pos=100: 1 + 1 = 2 bytes
        //     For index=5, pos=1000: 1 + 2 = 3 bytes

        let v2_position_size: usize = 41; // Option<Hash> + u64

        // Best case: all self-references at small offsets (1 + 1 = 2 bytes)
        let v3_best_case: usize = 2;
        let savings_best = (v2_position_size - v3_best_case) as f64 / v2_position_size as f64;
        assert!(savings_best > 0.95, "best case should save >95%");

        // Typical case: mixed self and deps at moderate offsets (~3 bytes)
        let v3_typical: usize = 3;
        let savings_typical = (v2_position_size - v3_typical) as f64 / v2_position_size as f64;
        assert!(savings_typical > 0.90, "typical case should save >90%");

        // Worst case: large index, large offset (3 + 5 = 8 bytes)
        let v3_worst_case: usize = 8;
        let savings_worst = (v2_position_size - v3_worst_case) as f64 / v2_position_size as f64;
        assert!(savings_worst > 0.80, "worst case should save >80%");
    }
}

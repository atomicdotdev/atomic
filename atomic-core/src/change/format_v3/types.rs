//! Core types for the Change Format V3 serialization layer.
//!
//! This module defines the foundational types used throughout the V3 format:
//!
//! - [`HashIndex`]: A `u16` reference into the hash deduplication table
//! - [`CompactPosition`]: A position using `HashIndex` instead of full 32-byte hashes
//! - [`SectionType`]: Discriminator for the different section kinds in a V3 file
//! - [`FileHeader`]: The fixed 64-byte header at the start of every V3 change file
//! - [`SectionHeader`]: The framing header for each section (type + compressed length)
//! - [`ContentChunkHeader`]: Extended header for content chunks (includes chunk hash)
//!
//! # Design Rationale
//!
//! ## Hash Deduplication
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash (plus 1 byte
//! for the `Option` discriminant = 33 bytes). A typical change references the same
//! few hashes (its own hash + dependency hashes) thousands of times throughout its
//! hunks. For an initial record of 194K LOC, this wastes ~18 MB.
//!
//! V3 stores unique hashes once in a dedup table at the top of the file, then
//! references them by `u16` index. This turns a 33-byte `Option<Hash>` into a
//! 1-3 byte postcard varint. Combined with postcard's varint encoding for the
//! position offset, a full `Position` shrinks from 41 bytes to 3-5 bytes.
//!
//! ## Section Types
//!
//! The file is divided into independently compressed sections, each tagged with
//! a `SectionType` byte. This enables:
//! - **Selective loading**: Read only GRAPH sections to apply, only SEMANTIC for review
//! - **Parallel compression**: Each section compresses independently
//! - **Streaming**: Process sections as they arrive over the network
//! - **Random access**: Seek to a specific section without deserializing everything
//!
//! ## Fixed Header
//!
//! The 64-byte header is intentionally fixed-size so readers can validate a file
//! with a single `read_exact(64)` call. The `reserved` field provides room for
//! future flags without changing the header size.
//!
//! # Wire Format
//!
//! ```text
//! FileHeader (64 bytes, fixed, uncompressed)
//! ├── magic: [u8; 4]            = b"ATOM"
//! ├── version: u32              = 1
//! ├── flags: u32                = bitfield (see FileHeaderFlags)
//! ├── hash_table_entries: u32   = count of unique hashes
//! ├── graph_section_count: u32  = number of GRAPH sections
//! ├── semantic_section_count: u32 = number of SEMANTIC sections
//! ├── contents_chunks: u32      = number of CONTENT chunks
//! ├── total_uncompressed: u64   = sum of all uncompressed section sizes
//! └── reserved: [u8; 28]        = zeros
//!
//! SectionHeader (5 bytes per section)
//! ├── section_type: u8          = SectionType discriminant
//! └── compressed_len: u32       = length of compressed payload
//!
//! ContentChunkHeader (41 bytes per content chunk)
//! ├── section_type: u8          = CONTENT (0x20)
//! ├── chunk_index: u32          = sequential chunk number
//! ├── chunk_hash: [u8; 32]      = blake3 of uncompressed chunk data
//! └── compressed_len: u32       = length of compressed payload
//! ```

use super::error::{FormatError, FormatResult, FORMAT_VERSION, MAGIC, MAX_HASH_TABLE_ENTRIES};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Read, Write};

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

// ═══════════════════════════════════════════════════════════════════════
// SectionType — discriminator for file sections
// ═══════════════════════════════════════════════════════════════════════

/// Identifies the type of a section in a V3 change file.
///
/// Each section in the file is preceded by a `SectionType` byte that tells
/// the reader what kind of data follows. Sections are grouped by layer:
///
/// | Range | Layer | Types |
/// |-------|-------|-------|
/// | `0x01-0x0F` | Metadata | HEADER, DEPS, PROVENANCE |
/// | `0x10-0x1F` | Graph | GRAPH (one per file) |
/// | `0x20-0x2F` | Content | CONTENT (content-defined chunks) |
/// | `0x30-0x3F` | Semantic | SEMANTIC (one per file) |
/// | `0xF0-0xFF` | Special | UNHASHED |
///
/// # Section Ordering
///
/// Sections MUST appear in this order within the file:
///
/// 1. `HEADER` (exactly one)
/// 2. `DEPS` (exactly one, may be empty)
/// 3. `PROVENANCE` (zero or one)
/// 4. `GRAPH` sections (zero or more, one per file)
/// 5. `SEMANTIC` sections (zero or more, one per file)
/// 6. `CONTENT` chunks (zero or more)
/// 7. `UNHASHED` (zero or one)
///
/// This ordering ensures that:
/// - The graph layer can be read without seeking past semantic/content sections
/// - A "thin pull" can stop reading after the last GRAPH section
/// - Content chunks come last for streaming write (they may still be compressing)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum SectionType {
    /// Change metadata (message, authors, timestamp).
    ///
    /// Payload: `zstd(postcard(ChangeHeader))`
    Header = 0x01,

    /// Dependency list (hashes of changes that must be applied first).
    ///
    /// Payload: `zstd(postcard(Vec<HashIndex>))`
    Dependencies = 0x02,

    /// AI provenance metadata (optional).
    ///
    /// Payload: `zstd(postcard(Vec<Provenance>))`
    Provenance = 0x03,

    /// Graph operations for a single file (storage/merge layer).
    ///
    /// There is one GRAPH section per modified file. Each section contains
    /// the graph operations (vertex insertions, edge updates) needed to
    /// apply the change to that file's DAG.
    ///
    /// Payload: `zstd(postcard(GraphSection))`
    Graph = 0x10,

    /// Content chunk (content-defined, independently compressed).
    ///
    /// Content is split into variable-size chunks using FastCDC. Each chunk
    /// is independently compressed and content-addressed by its blake3 hash.
    /// This enables delta transfer — only chunks the receiver doesn't have
    /// need to be sent.
    ///
    /// Content chunks have an extended header with chunk index and hash.
    ///
    /// Payload: `zstd(raw content bytes)`
    Content = 0x20,

    /// Semantic operations for a single file (display/analysis layer).
    ///
    /// There is one SEMANTIC section per modified file. Each section contains
    /// the Trunk/Branch/Leaf operations for line-level and token-level diffs,
    /// blame, and code review.
    ///
    /// SEMANTIC sections are independently loadable — a code review UI can
    /// read only these sections without touching GRAPH sections.
    ///
    /// Payload: `zstd(postcard(SemanticSection))`
    Semantic = 0x30,

    /// Unhashed metadata (not included in the change hash).
    ///
    /// This section holds data that shouldn't affect the change's identity:
    /// AI transcripts, reasoning traces, editor state, review comments, etc.
    /// It's always the last hashed section (well, it's actually unhashed).
    ///
    /// Payload: `zstd(json(Value))`
    Unhashed = 0xF0,
}

impl SectionType {
    /// Convert a raw byte to a `SectionType`.
    ///
    /// Returns `Err(FormatError::InvalidSectionType)` if the byte doesn't
    /// match any known section type.
    ///
    /// # Arguments
    ///
    /// * `byte` - The raw section type byte from the file
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::SectionType;
    ///
    /// assert_eq!(SectionType::from_byte(0x01).unwrap(), SectionType::Header);
    /// assert_eq!(SectionType::from_byte(0x10).unwrap(), SectionType::Graph);
    /// assert!(SectionType::from_byte(0xFF).is_err());
    /// ```
    pub fn from_byte(byte: u8) -> FormatResult<Self> {
        match byte {
            0x01 => Ok(SectionType::Header),
            0x02 => Ok(SectionType::Dependencies),
            0x03 => Ok(SectionType::Provenance),
            0x10 => Ok(SectionType::Graph),
            0x20 => Ok(SectionType::Content),
            0x30 => Ok(SectionType::Semantic),
            0xF0 => Ok(SectionType::Unhashed),
            _ => Err(FormatError::InvalidSectionType { type_byte: byte }),
        }
    }

    /// Convert this section type to its raw byte representation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::SectionType;
    ///
    /// assert_eq!(SectionType::Header.to_byte(), 0x01);
    /// assert_eq!(SectionType::Graph.to_byte(), 0x10);
    /// assert_eq!(SectionType::Unhashed.to_byte(), 0xF0);
    /// ```
    #[inline]
    pub const fn to_byte(self) -> u8 {
        self as u8
    }

    /// Returns `true` if this section type is part of the hashed content.
    ///
    /// All sections except `Unhashed` contribute to the change's content hash.
    /// The hash is computed incrementally as hashed sections are written.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::SectionType;
    ///
    /// assert!(SectionType::Header.is_hashed());
    /// assert!(SectionType::Graph.is_hashed());
    /// assert!(SectionType::Semantic.is_hashed());
    /// assert!(SectionType::Content.is_hashed());
    /// assert!(!SectionType::Unhashed.is_hashed());
    /// ```
    #[inline]
    pub const fn is_hashed(self) -> bool {
        !matches!(self, SectionType::Unhashed)
    }

    /// Returns `true` if this is a metadata section (HEADER, DEPS, PROVENANCE).
    ///
    /// Metadata sections appear first in the file and contain information
    /// about the change itself rather than file modifications.
    #[inline]
    pub const fn is_metadata(self) -> bool {
        matches!(
            self,
            SectionType::Header | SectionType::Dependencies | SectionType::Provenance
        )
    }

    /// Returns `true` if this is a per-file section (GRAPH or SEMANTIC).
    ///
    /// Per-file sections are repeated for each modified file in the change.
    #[inline]
    pub const fn is_per_file(self) -> bool {
        matches!(self, SectionType::Graph | SectionType::Semantic)
    }

    /// Returns a human-readable name for this section type.
    ///
    /// Used in error messages and debug output.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::SectionType;
    ///
    /// assert_eq!(SectionType::Header.name(), "HEADER");
    /// assert_eq!(SectionType::Graph.name(), "GRAPH");
    /// assert_eq!(SectionType::Content.name(), "CONTENT");
    /// ```
    pub const fn name(self) -> &'static str {
        match self {
            SectionType::Header => "HEADER",
            SectionType::Dependencies => "DEPS",
            SectionType::Provenance => "PROVENANCE",
            SectionType::Graph => "GRAPH",
            SectionType::Content => "CONTENT",
            SectionType::Semantic => "SEMANTIC",
            SectionType::Unhashed => "UNHASHED",
        }
    }

    /// Returns the expected ordering index for this section type.
    ///
    /// Lower values must appear earlier in the file. Sections with the
    /// same ordering index (like multiple GRAPH sections) can appear in
    /// any relative order within their group.
    ///
    /// Used internally to validate section ordering during reading.
    pub const fn ordering(self) -> u8 {
        match self {
            SectionType::Header => 0,
            SectionType::Dependencies => 1,
            SectionType::Provenance => 2,
            SectionType::Graph => 3,
            SectionType::Semantic => 4,
            SectionType::Content => 5,
            SectionType::Unhashed => 6,
        }
    }
}

impl fmt::Display for SectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FileHeaderFlags — bitfield for header flags
// ═══════════════════════════════════════════════════════════════════════

/// Bitfield flags stored in the file header's `flags` field.
///
/// These flags communicate optional features or characteristics of the
/// change file without changing the overall format structure.
///
/// # Bit Assignments
///
/// | Bit | Name | Meaning |
/// |-----|------|---------|
/// | 0 | `HAS_PROVENANCE` | Provenance section is present |
/// | 1 | `HAS_SEMANTIC` | Semantic sections are present |
/// | 2 | `HAS_UNHASHED` | Unhashed section is present |
/// | 3-31 | Reserved | Must be zero |
///
/// # Forward Compatibility
///
/// Readers MUST ignore unknown flags (bits 3-31). This allows newer writers
/// to set flags that older readers don't understand without breaking them.
/// If a future flag requires breaking changes, the `version` field should
/// be incremented instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileHeaderFlags(u32);

impl FileHeaderFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);

    /// Provenance section is present in the file.
    pub const HAS_PROVENANCE: u32 = 1 << 0;

    /// Semantic sections are present in the file.
    pub const HAS_SEMANTIC: u32 = 1 << 1;

    /// Unhashed section is present in the file.
    pub const HAS_UNHASHED: u32 = 1 << 2;

    /// Mask of all known flags (for validation).
    const KNOWN_MASK: u32 = Self::HAS_PROVENANCE | Self::HAS_SEMANTIC | Self::HAS_UNHASHED;

    /// Create flags from a raw `u32` value.
    ///
    /// Unknown bits are preserved but can be queried with [`has_unknown_flags`](Self::has_unknown_flags).
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Get the raw `u32` value.
    #[inline]
    pub const fn to_raw(self) -> u32 {
        self.0
    }

    /// Returns `true` if the given flag bit is set.
    #[inline]
    pub const fn has(self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }

    /// Set a flag bit.
    #[inline]
    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    /// Clear a flag bit.
    #[inline]
    pub fn clear(&mut self, flag: u32) {
        self.0 &= !flag;
    }

    /// Returns `true` if any unknown (reserved) flag bits are set.
    ///
    /// This is informational — unknown flags should be tolerated, not rejected.
    #[inline]
    pub const fn has_unknown_flags(self) -> bool {
        (self.0 & !Self::KNOWN_MASK) != 0
    }

    /// Returns `true` if no flags are set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for FileHeaderFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.has(Self::HAS_PROVENANCE) {
            parts.push("PROVENANCE");
        }
        if self.has(Self::HAS_SEMANTIC) {
            parts.push("SEMANTIC");
        }
        if self.has(Self::HAS_UNHASHED) {
            parts.push("UNHASHED");
        }
        if parts.is_empty() {
            write!(f, "(none)")
        } else {
            write!(f, "{}", parts.join(" | "))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FileHeader — the fixed 64-byte header
// ═══════════════════════════════════════════════════════════════════════

/// The fixed 64-byte header at the start of every V3 change file.
///
/// This header can be read with a single `read_exact(64)` call and provides
/// all the information needed to plan the rest of the read:
///
/// - How many sections to expect (graph, semantic, content)
/// - Whether optional sections exist (provenance, unhashed)
/// - Total uncompressed size (for progress reporting)
///
/// # Wire Format
///
/// ```text
/// Offset  Size  Field
/// ──────  ────  ─────
///   0       4   magic: b"ATOM"
///   4       4   version: u32 LE (= 1)
///   8       4   flags: u32 LE (FileHeaderFlags bitfield)
///  12       4   hash_table_entries: u32 LE
///  16       4   graph_section_count: u32 LE
///  20       4   semantic_section_count: u32 LE
///  24       4   contents_chunks: u32 LE
///  28       8   total_uncompressed: u64 LE
///  36      28   reserved: [0u8; 28]
///  ──────────
///  Total: 64 bytes
/// ```
///
/// All multi-byte integers are little-endian, matching the platform
/// convention used throughout Atomic (see `L64` type).
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::{FileHeader, FileHeaderFlags};
///
/// // Build a header for a change with 3 files
/// let header = FileHeader::builder()
///     .hash_table_entries(5)
///     .graph_section_count(3)
///     .semantic_section_count(3)
///     .contents_chunks(10)
///     .total_uncompressed(1024 * 1024)
///     .build();
///
/// assert_eq!(header.graph_section_count, 3);
/// assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
///
/// // Serialize to bytes and back
/// let bytes = header.to_bytes();
/// assert_eq!(bytes.len(), FileHeader::SIZE);
/// let decoded = FileHeader::from_bytes(&bytes).unwrap();
/// assert_eq!(decoded, header);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    /// Magic bytes — always `b"ATOM"`.
    pub magic: [u8; 4],

    /// Format version — always `1` for V3.
    pub version: u32,

    /// Feature flags (see [`FileHeaderFlags`]).
    pub flags: FileHeaderFlags,

    /// Number of entries in the hash deduplication table.
    ///
    /// The hash table immediately follows this header and contains this
    /// many 32-byte Blake3 hashes. Index 0 is always the change's own hash.
    pub hash_table_entries: u32,

    /// Number of GRAPH sections in the file (one per modified file).
    pub graph_section_count: u32,

    /// Number of SEMANTIC sections in the file (one per modified file).
    ///
    /// This can be zero for a "thin" change that omits the semantic layer.
    /// Semantic sections can be regenerated from graph + content.
    pub semantic_section_count: u32,

    /// Number of content chunks in the file.
    ///
    /// Content is split into variable-size chunks using FastCDC. Each chunk
    /// is independently compressed and content-addressed.
    pub contents_chunks: u32,

    /// Total uncompressed size of all sections, in bytes.
    ///
    /// This is used for progress reporting during read/write operations.
    /// It includes all sections (metadata + graph + semantic + content)
    /// but NOT the file header, hash table, or trailer.
    pub total_uncompressed: u64,

    /// Reserved bytes for future use — must be zero.
    ///
    /// Readers MUST ignore non-zero values in this field (forward compat).
    /// Writers MUST set this to all zeros.
    pub reserved: [u8; 28],
}

impl FileHeader {
    /// Total size of the serialized header in bytes.
    pub const SIZE: usize = 64;

    /// Serialize this header to a 64-byte array.
    ///
    /// All multi-byte fields are little-endian.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::FileHeader;
    ///
    /// let header = FileHeader::default();
    /// let bytes = header.to_bytes();
    /// assert_eq!(bytes.len(), 64);
    /// assert_eq!(&bytes[0..4], b"ATOM");
    /// ```
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.flags.to_raw().to_le_bytes());
        buf[12..16].copy_from_slice(&self.hash_table_entries.to_le_bytes());
        buf[16..20].copy_from_slice(&self.graph_section_count.to_le_bytes());
        buf[20..24].copy_from_slice(&self.semantic_section_count.to_le_bytes());
        buf[24..28].copy_from_slice(&self.contents_chunks.to_le_bytes());
        buf[28..36].copy_from_slice(&self.total_uncompressed.to_le_bytes());
        // reserved is already zeroed
        buf
    }

    /// Deserialize a header from a 64-byte array.
    ///
    /// Validates the magic bytes and version field. Returns an error if
    /// the magic doesn't match or the version is unsupported.
    ///
    /// # Errors
    ///
    /// - [`FormatError::InvalidMagic`] if the first 4 bytes aren't `b"ATOM"`
    /// - [`FormatError::UnsupportedVersion`] if the version isn't `1`
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::FileHeader;
    ///
    /// let header = FileHeader::default();
    /// let bytes = header.to_bytes();
    /// let decoded = FileHeader::from_bytes(&bytes).unwrap();
    /// assert_eq!(decoded.version, 1);
    /// ```
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> FormatResult<Self> {
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != MAGIC {
            return Err(FormatError::InvalidMagic { got: magic });
        }

        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                expected: FORMAT_VERSION,
                got: version,
            });
        }

        let flags = FileHeaderFlags::from_raw(u32::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
        ]));
        let hash_table_entries = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let graph_section_count = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let semantic_section_count =
            u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let contents_chunks = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let total_uncompressed = u64::from_le_bytes([
            bytes[28], bytes[29], bytes[30], bytes[31], bytes[32], bytes[33], bytes[34], bytes[35],
        ]);

        let mut reserved = [0u8; 28];
        reserved.copy_from_slice(&bytes[36..64]);

        Ok(Self {
            magic,
            version,
            flags,
            hash_table_entries,
            graph_section_count,
            semantic_section_count,
            contents_chunks,
            total_uncompressed,
            reserved,
        })
    }

    /// Write this header to a writer.
    ///
    /// Writes exactly 64 bytes.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> FormatResult<()> {
        writer.write_all(&self.to_bytes())?;
        Ok(())
    }

    /// Read a header from a reader.
    ///
    /// Reads exactly 64 bytes and validates magic + version.
    ///
    /// # Errors
    ///
    /// - I/O error if fewer than 64 bytes are available
    /// - [`FormatError::InvalidMagic`] if magic doesn't match
    /// - [`FormatError::UnsupportedVersion`] if version isn't supported
    pub fn read_from<R: Read>(reader: &mut R) -> FormatResult<Self> {
        let mut buf = [0u8; Self::SIZE];
        reader.read_exact(&mut buf)?;
        Self::from_bytes(&buf)
    }

    /// Create a builder for constructing a `FileHeader`.
    ///
    /// The builder sets sensible defaults (magic = `b"ATOM"`, version = 1,
    /// flags = auto-computed, reserved = zeros) and lets you configure
    /// the section counts.
    pub fn builder() -> FileHeaderBuilder {
        FileHeaderBuilder::new()
    }

    /// Returns the total number of sections in the file (excluding header and trailer).
    ///
    /// This is `1 (HEADER) + 1 (DEPS) + provenance? + graph + semantic + content + unhashed?`.
    pub fn total_section_count(&self) -> u32 {
        let mut count = 2; // HEADER + DEPS always present
        if self.flags.has(FileHeaderFlags::HAS_PROVENANCE) {
            count += 1;
        }
        count += self.graph_section_count;
        count += self.semantic_section_count;
        count += self.contents_chunks;
        if self.flags.has(FileHeaderFlags::HAS_UNHASHED) {
            count += 1;
        }
        count
    }

    /// Validate that the header fields are internally consistent.
    ///
    /// # Checks
    ///
    /// - `hash_table_entries` <= `MAX_HASH_TABLE_ENTRIES`
    /// - If `HAS_SEMANTIC` flag is set, `semantic_section_count > 0`
    /// - If `semantic_section_count > 0`, `HAS_SEMANTIC` flag must be set
    pub fn validate(&self) -> FormatResult<()> {
        if self.hash_table_entries as usize > MAX_HASH_TABLE_ENTRIES {
            return Err(FormatError::InvalidHeader {
                reason: format!(
                    "hash_table_entries ({}) exceeds maximum ({})",
                    self.hash_table_entries, MAX_HASH_TABLE_ENTRIES
                ),
            });
        }

        if self.flags.has(FileHeaderFlags::HAS_SEMANTIC) && self.semantic_section_count == 0 {
            return Err(FormatError::InvalidHeader {
                reason: "HAS_SEMANTIC flag is set but semantic_section_count is 0".to_string(),
            });
        }

        if self.semantic_section_count > 0 && !self.flags.has(FileHeaderFlags::HAS_SEMANTIC) {
            return Err(FormatError::InvalidHeader {
                reason: format!(
                    "semantic_section_count is {} but HAS_SEMANTIC flag is not set",
                    self.semantic_section_count
                ),
            });
        }

        Ok(())
    }
}

impl Default for FileHeader {
    /// Default header with valid magic and version, zero counts.
    fn default() -> Self {
        Self {
            magic: MAGIC,
            version: FORMAT_VERSION,
            flags: FileHeaderFlags::NONE,
            hash_table_entries: 0,
            graph_section_count: 0,
            semantic_section_count: 0,
            contents_chunks: 0,
            total_uncompressed: 0,
            reserved: [0u8; 28],
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FileHeaderBuilder — fluent builder for FileHeader
// ═══════════════════════════════════════════════════════════════════════

/// Fluent builder for constructing [`FileHeader`] values.
///
/// The builder auto-computes the `flags` field based on the section counts
/// you provide. For example, setting `semantic_section_count(3)` automatically
/// sets the `HAS_SEMANTIC` flag.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::{FileHeader, FileHeaderFlags};
///
/// let header = FileHeader::builder()
///     .hash_table_entries(2)
///     .graph_section_count(5)
///     .semantic_section_count(5)
///     .contents_chunks(20)
///     .total_uncompressed(10 * 1024 * 1024)
///     .build();
///
/// assert_eq!(header.graph_section_count, 5);
/// assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
/// ```
pub struct FileHeaderBuilder {
    hash_table_entries: u32,
    graph_section_count: u32,
    semantic_section_count: u32,
    contents_chunks: u32,
    total_uncompressed: u64,
    has_provenance: bool,
    has_unhashed: bool,
}

impl FileHeaderBuilder {
    /// Create a new builder with default values (all counts zero).
    pub fn new() -> Self {
        Self {
            hash_table_entries: 0,
            graph_section_count: 0,
            semantic_section_count: 0,
            contents_chunks: 0,
            total_uncompressed: 0,
            has_provenance: false,
            has_unhashed: false,
        }
    }

    /// Set the number of unique hashes in the dedup table.
    pub fn hash_table_entries(mut self, count: u32) -> Self {
        self.hash_table_entries = count;
        self
    }

    /// Set the number of GRAPH sections (one per modified file).
    pub fn graph_section_count(mut self, count: u32) -> Self {
        self.graph_section_count = count;
        self
    }

    /// Set the number of SEMANTIC sections (one per modified file).
    ///
    /// Automatically sets the `HAS_SEMANTIC` flag if count > 0.
    pub fn semantic_section_count(mut self, count: u32) -> Self {
        self.semantic_section_count = count;
        self
    }

    /// Set the number of content chunks.
    pub fn contents_chunks(mut self, count: u32) -> Self {
        self.contents_chunks = count;
        self
    }

    /// Set the total uncompressed size for progress reporting.
    pub fn total_uncompressed(mut self, size: u64) -> Self {
        self.total_uncompressed = size;
        self
    }

    /// Mark that this change has a provenance section.
    ///
    /// Automatically sets the `HAS_PROVENANCE` flag.
    pub fn with_provenance(mut self) -> Self {
        self.has_provenance = true;
        self
    }

    /// Mark that this change has an unhashed section.
    ///
    /// Automatically sets the `HAS_UNHASHED` flag.
    pub fn with_unhashed(mut self) -> Self {
        self.has_unhashed = true;
        self
    }

    /// Build the [`FileHeader`], auto-computing flags.
    pub fn build(self) -> FileHeader {
        let mut flags = FileHeaderFlags::NONE;
        if self.has_provenance {
            flags.set(FileHeaderFlags::HAS_PROVENANCE);
        }
        if self.semantic_section_count > 0 {
            flags.set(FileHeaderFlags::HAS_SEMANTIC);
        }
        if self.has_unhashed {
            flags.set(FileHeaderFlags::HAS_UNHASHED);
        }

        FileHeader {
            magic: MAGIC,
            version: FORMAT_VERSION,
            flags,
            hash_table_entries: self.hash_table_entries,
            graph_section_count: self.graph_section_count,
            semantic_section_count: self.semantic_section_count,
            contents_chunks: self.contents_chunks,
            total_uncompressed: self.total_uncompressed,
            reserved: [0u8; 28],
        }
    }
}

impl Default for FileHeaderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SectionHeader — framing for each section
// ═══════════════════════════════════════════════════════════════════════

/// Framing header for a single section in the V3 change file.
///
/// Every section (except content chunks, which use [`ContentChunkHeader`])
/// is preceded by this 5-byte header:
///
/// ```text
/// ┌──────────────────┬──────────────────────┐
/// │ section_type: u8 │ compressed_len: u32  │
/// │   (1 byte)       │   (4 bytes, LE)      │
/// └──────────────────┴──────────────────────┘
/// ```
///
/// After this header, exactly `compressed_len` bytes of zstd-compressed
/// data follow. The reader decompresses those bytes to get the section
/// payload, which is postcard-serialized data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    /// The type of section.
    pub section_type: SectionType,

    /// Length of the compressed payload that follows this header, in bytes.
    pub compressed_len: u32,
}

impl SectionHeader {
    /// Size of a serialized section header in bytes.
    pub const SIZE: usize = 5;

    /// Create a new section header.
    #[inline]
    pub const fn new(section_type: SectionType, compressed_len: u32) -> Self {
        Self {
            section_type,
            compressed_len,
        }
    }

    /// Serialize to a 5-byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0] = self.section_type.to_byte();
        buf[1..5].copy_from_slice(&self.compressed_len.to_le_bytes());
        buf
    }

    /// Deserialize from a 5-byte array.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::InvalidSectionType`] if the type byte is unknown.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> FormatResult<Self> {
        let section_type = SectionType::from_byte(bytes[0])?;
        let compressed_len = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        Ok(Self {
            section_type,
            compressed_len,
        })
    }

    /// Write this header to a writer (5 bytes).
    pub fn write_to<W: Write>(&self, writer: &mut W) -> FormatResult<()> {
        writer.write_all(&self.to_bytes())?;
        Ok(())
    }

    /// Read a section header from a reader (5 bytes).
    pub fn read_from<R: Read>(reader: &mut R) -> FormatResult<Self> {
        let mut buf = [0u8; Self::SIZE];
        reader.read_exact(&mut buf)?;
        Self::from_bytes(&buf)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ContentChunkHeader — extended header for content chunks
// ═══════════════════════════════════════════════════════════════════════

/// Extended header for content chunks, which carry additional metadata.
///
/// Content chunks differ from other sections because they are:
/// - **Content-addressed**: Each chunk has a blake3 hash of its uncompressed data
/// - **Indexed**: Each chunk has a sequential index for ordering
/// - **Deduplicatable**: The hash enables delta transfer (skip chunks the receiver has)
///
/// # Wire Format
///
/// ```text
/// ┌──────────────────┬──────────────────┬──────────────────────────┬────────────────────────┬──────────────────────┐
/// │ section_type: u8 │ chunk_index: u32 │ chunk_hash: [u8; 32]     │ uncompressed_len: u32  │ compressed_len: u32  │
/// │   (= 0x20)       │   (4 bytes, LE)  │   (32 bytes)             │   (4 bytes, LE)        │   (4 bytes, LE)      │
/// └──────────────────┴──────────────────┴──────────────────────────┴────────────────────────┴──────────────────────┘
/// Total: 45 bytes
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentChunkHeader {
    /// Sequential chunk index (0-based).
    pub chunk_index: u32,

    /// Blake3 hash of the uncompressed chunk data.
    ///
    /// This is used for:
    /// - Delta transfer (identify chunks the receiver already has)
    /// - Integrity verification
    /// - Cross-change deduplication
    pub chunk_hash: [u8; 32],

    /// Size of the uncompressed chunk data, in bytes.
    pub uncompressed_len: u32,

    /// Size of the compressed chunk data that follows this header.
    pub compressed_len: u32,
}

impl ContentChunkHeader {
    /// Size of a serialized content chunk header in bytes.
    pub const SIZE: usize = 45; // 1 + 4 + 32 + 4 + 4

    /// Create a new content chunk header.
    pub const fn new(
        chunk_index: u32,
        chunk_hash: [u8; 32],
        uncompressed_len: u32,
        compressed_len: u32,
    ) -> Self {
        Self {
            chunk_index,
            chunk_hash,
            uncompressed_len,
            compressed_len,
        }
    }

    /// Serialize to a byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0] = SectionType::Content.to_byte();
        buf[1..5].copy_from_slice(&self.chunk_index.to_le_bytes());
        buf[5..37].copy_from_slice(&self.chunk_hash);
        buf[37..41].copy_from_slice(&self.uncompressed_len.to_le_bytes());
        buf[41..45].copy_from_slice(&self.compressed_len.to_le_bytes());
        buf
    }

    /// Deserialize from a byte array.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::InvalidSectionType`] if the first byte isn't `0x20` (CONTENT).
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> FormatResult<Self> {
        let section_type = SectionType::from_byte(bytes[0])?;
        if section_type != SectionType::Content {
            return Err(FormatError::InvalidSectionType {
                type_byte: bytes[0],
            });
        }

        let chunk_index = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let mut chunk_hash = [0u8; 32];
        chunk_hash.copy_from_slice(&bytes[5..37]);
        let uncompressed_len = u32::from_le_bytes([bytes[37], bytes[38], bytes[39], bytes[40]]);
        let compressed_len = u32::from_le_bytes([bytes[41], bytes[42], bytes[43], bytes[44]]);

        Ok(Self {
            chunk_index,
            chunk_hash,
            uncompressed_len,
            compressed_len,
        })
    }

    /// Write this header to a writer.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> FormatResult<()> {
        writer.write_all(&self.to_bytes())?;
        Ok(())
    }

    /// Read a content chunk header from a reader.
    ///
    /// Expects the section type byte to have already been peeked or to be
    /// the first byte read. Reads exactly [`Self::SIZE`] bytes.
    pub fn read_from<R: Read>(reader: &mut R) -> FormatResult<Self> {
        let mut buf = [0u8; Self::SIZE];
        reader.read_exact(&mut buf)?;
        Self::from_bytes(&buf)
    }

    /// Compression ratio (compressed_len / uncompressed_len).
    ///
    /// Returns `f64::NAN` if `uncompressed_len` is zero.
    pub fn compression_ratio(&self) -> f64 {
        if self.uncompressed_len == 0 {
            return f64::NAN;
        }
        self.compressed_len as f64 / self.uncompressed_len as f64
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Trailer — the final 32 bytes of the file
// ═══════════════════════════════════════════════════════════════════════

/// The trailer at the end of every V3 change file.
///
/// Contains the blake3 hash computed incrementally over all hashed sections.
/// This hash is the **identity** of the change — it's what gets registered
/// in the pristine database and used for content addressing.
///
/// # Hash Coverage
///
/// The content hash covers (in order):
/// 1. Hash dedup table bytes
/// 2. HEADER section (compressed)
/// 3. DEPS section (compressed)
/// 4. PROVENANCE section (compressed, if present)
/// 5. All GRAPH sections (compressed, in file order)
/// 6. All SEMANTIC sections (compressed, in file order)
/// 7. All CONTENT chunks (compressed, in file order)
///
/// The UNHASHED section is explicitly NOT included in the hash.
/// The file header and trailer are also NOT included (the header contains
/// structural metadata that could change without changing the semantic
/// content, and the trailer contains the hash itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// Blake3 hash of all hashed sections, computed incrementally.
    pub content_hash: [u8; 32],
}

impl Trailer {
    /// Size of the trailer in bytes.
    pub const SIZE: usize = 32;

    /// Serialize to a 32-byte array.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        self.content_hash
    }

    /// Deserialize from a 32-byte array.
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(bytes);
        Self { content_hash }
    }

    /// Write to a writer.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> FormatResult<()> {
        writer.write_all(&self.content_hash)?;
        Ok(())
    }

    /// Read from a reader.
    pub fn read_from<R: Read>(reader: &mut R) -> FormatResult<Self> {
        let mut buf = [0u8; Self::SIZE];
        reader.read_exact(&mut buf)?;
        Ok(Self::from_bytes(&buf))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── HashIndex ──────────────────────────────────────────────────

    #[test]
    fn test_hash_index_none_value() {
        assert_eq!(HASH_INDEX_NONE, 0xFFFF);
        assert_eq!(HASH_INDEX_NONE, u16::MAX);
    }

    #[test]
    fn test_hash_index_self_value() {
        assert_eq!(HASH_INDEX_SELF, 0);
    }

    #[test]
    fn test_is_none_index() {
        assert!(is_none_index(HASH_INDEX_NONE));
        assert!(!is_none_index(HASH_INDEX_SELF));
        assert!(!is_none_index(1));
        assert!(!is_none_index(0xFFFE));
    }

    #[test]
    fn test_hash_index_range() {
        // Valid indices: 0 through 0xFFFE (65534 values)
        // Reserved: 0xFFFF (NONE sentinel)
        assert_eq!(MAX_HASH_TABLE_ENTRIES, 65534);
        assert_eq!(HASH_INDEX_NONE as usize, MAX_HASH_TABLE_ENTRIES + 1);
    }

    // ── CompactPosition ───────────────────────────────────────────

    #[test]
    fn test_compact_position_new() {
        let pos = CompactPosition::new(5, 100);
        assert_eq!(pos.change, 5);
        assert_eq!(pos.pos, 100);
    }

    #[test]
    fn test_compact_position_root() {
        let pos = CompactPosition::root(42);
        assert_eq!(pos.change, HASH_INDEX_NONE);
        assert_eq!(pos.pos, 42);
        assert!(pos.is_root());
        assert!(!pos.is_self_ref());
    }

    #[test]
    fn test_compact_position_self_ref() {
        let pos = CompactPosition::self_ref(99);
        assert_eq!(pos.change, HASH_INDEX_SELF);
        assert_eq!(pos.pos, 99);
        assert!(!pos.is_root());
        assert!(pos.is_self_ref());
    }

    #[test]
    fn test_compact_position_dependency_ref() {
        let pos = CompactPosition::new(3, 500);
        assert!(!pos.is_root());
        assert!(!pos.is_self_ref());
        assert_eq!(pos.change, 3);
        assert_eq!(pos.pos, 500);
    }

    #[test]
    fn test_compact_position_display_root() {
        let pos = CompactPosition::root(0);
        assert_eq!(format!("{}", pos), "ROOT:0");

        let pos = CompactPosition::root(42);
        assert_eq!(format!("{}", pos), "ROOT:42");
    }

    #[test]
    fn test_compact_position_display_self() {
        let pos = CompactPosition::self_ref(100);
        assert_eq!(format!("{}", pos), "SELF:100");
    }

    #[test]
    fn test_compact_position_display_dependency() {
        let pos = CompactPosition::new(7, 256);
        assert_eq!(format!("{}", pos), "#7:256");
    }

    #[test]
    fn test_compact_position_equality() {
        let a = CompactPosition::new(1, 10);
        let b = CompactPosition::new(1, 10);
        let c = CompactPosition::new(1, 20);
        let d = CompactPosition::new(2, 10);

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn test_compact_position_ordering() {
        let positions = vec![
            CompactPosition::new(2, 10),
            CompactPosition::new(1, 20),
            CompactPosition::new(1, 10),
            CompactPosition::root(0),
        ];

        let mut sorted = positions.clone();
        sorted.sort();

        // Sorted by change index first, then pos
        assert_eq!(sorted[0], CompactPosition::new(1, 10));
        assert_eq!(sorted[1], CompactPosition::new(1, 20));
        assert_eq!(sorted[2], CompactPosition::new(2, 10));
        // HASH_INDEX_NONE (0xFFFF) sorts last
        assert_eq!(sorted[3], CompactPosition::root(0));
    }

    #[test]
    fn test_compact_position_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(CompactPosition::self_ref(10));
        set.insert(CompactPosition::self_ref(10)); // duplicate
        set.insert(CompactPosition::self_ref(20));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_compact_position_postcard_roundtrip() {
        let positions = vec![
            CompactPosition::root(0),
            CompactPosition::self_ref(0),
            CompactPosition::self_ref(42),
            CompactPosition::new(1, 100),
            CompactPosition::new(100, 10000),
            CompactPosition::new(0xFFFE, u32::MAX),
        ];

        for pos in &positions {
            let bytes = postcard::to_allocvec(pos).expect("serialize");
            let decoded: CompactPosition = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(*pos, decoded, "roundtrip failed for {:?}", pos);
        }
    }

    #[test]
    fn test_compact_position_postcard_size() {
        // Index 0, pos 0 → both varint(0) = 1 byte each → 2 bytes total
        let small = CompactPosition::self_ref(0);
        let bytes = postcard::to_allocvec(&small).unwrap();
        assert_eq!(bytes.len(), 2, "SELF:0 should be 2 bytes in postcard");

        // Index 0, pos 42 → varint(0) + varint(42) = 1 + 1 = 2 bytes
        let medium = CompactPosition::self_ref(42);
        let bytes = postcard::to_allocvec(&medium).unwrap();
        assert_eq!(bytes.len(), 2, "SELF:42 should be 2 bytes in postcard");

        // Index 0, pos 200 → varint(0) + varint(200) = 1 + 2 = 3 bytes
        let larger = CompactPosition::self_ref(200);
        let bytes = postcard::to_allocvec(&larger).unwrap();
        assert!(bytes.len() <= 3, "SELF:200 should be at most 3 bytes");

        // Compare with what bincode Option<Hash> + u64 would cost: 33 + 8 = 41 bytes
        // We're at 2-3 bytes. That's a 90%+ reduction.
    }

    #[test]
    fn test_compact_position_max_values() {
        // Maximum valid index (0xFFFE) and maximum pos (u32::MAX)
        let max = CompactPosition::new(0xFFFE, u32::MAX);
        let bytes = postcard::to_allocvec(&max).unwrap();
        let decoded: CompactPosition = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(max, decoded);
        // Even max values should be reasonable size (3 + 5 = 8 bytes max)
        assert!(bytes.len() <= 8);
    }

    // ── SectionType ───────────────────────────────────────────────

    #[test]
    fn test_section_type_byte_values() {
        assert_eq!(SectionType::Header.to_byte(), 0x01);
        assert_eq!(SectionType::Dependencies.to_byte(), 0x02);
        assert_eq!(SectionType::Provenance.to_byte(), 0x03);
        assert_eq!(SectionType::Graph.to_byte(), 0x10);
        assert_eq!(SectionType::Content.to_byte(), 0x20);
        assert_eq!(SectionType::Semantic.to_byte(), 0x30);
        assert_eq!(SectionType::Unhashed.to_byte(), 0xF0);
    }

    #[test]
    fn test_section_type_from_byte_roundtrip() {
        let types = [
            SectionType::Header,
            SectionType::Dependencies,
            SectionType::Provenance,
            SectionType::Graph,
            SectionType::Content,
            SectionType::Semantic,
            SectionType::Unhashed,
        ];

        for st in &types {
            let byte = st.to_byte();
            let decoded = SectionType::from_byte(byte).unwrap();
            assert_eq!(*st, decoded, "roundtrip failed for {:?}", st);
        }
    }

    #[test]
    fn test_section_type_from_byte_invalid() {
        // Test some invalid bytes
        for byte in [0x00, 0x04, 0x0F, 0x11, 0x21, 0x31, 0xFF] {
            let result = SectionType::from_byte(byte);
            assert!(result.is_err(), "byte 0x{:02X} should be invalid", byte);
        }
    }

    #[test]
    fn test_section_type_is_hashed() {
        assert!(SectionType::Header.is_hashed());
        assert!(SectionType::Dependencies.is_hashed());
        assert!(SectionType::Provenance.is_hashed());
        assert!(SectionType::Graph.is_hashed());
        assert!(SectionType::Content.is_hashed());
        assert!(SectionType::Semantic.is_hashed());

        // Only Unhashed is not hashed
        assert!(!SectionType::Unhashed.is_hashed());
    }

    #[test]
    fn test_section_type_is_metadata() {
        assert!(SectionType::Header.is_metadata());
        assert!(SectionType::Dependencies.is_metadata());
        assert!(SectionType::Provenance.is_metadata());

        assert!(!SectionType::Graph.is_metadata());
        assert!(!SectionType::Content.is_metadata());
        assert!(!SectionType::Semantic.is_metadata());
        assert!(!SectionType::Unhashed.is_metadata());
    }

    #[test]
    fn test_section_type_is_per_file() {
        assert!(SectionType::Graph.is_per_file());
        assert!(SectionType::Semantic.is_per_file());

        assert!(!SectionType::Header.is_per_file());
        assert!(!SectionType::Dependencies.is_per_file());
        assert!(!SectionType::Provenance.is_per_file());
        assert!(!SectionType::Content.is_per_file());
        assert!(!SectionType::Unhashed.is_per_file());
    }

    #[test]
    fn test_section_type_name() {
        assert_eq!(SectionType::Header.name(), "HEADER");
        assert_eq!(SectionType::Dependencies.name(), "DEPS");
        assert_eq!(SectionType::Provenance.name(), "PROVENANCE");
        assert_eq!(SectionType::Graph.name(), "GRAPH");
        assert_eq!(SectionType::Content.name(), "CONTENT");
        assert_eq!(SectionType::Semantic.name(), "SEMANTIC");
        assert_eq!(SectionType::Unhashed.name(), "UNHASHED");
    }

    #[test]
    fn test_section_type_display() {
        assert_eq!(format!("{}", SectionType::Graph), "GRAPH");
        assert_eq!(format!("{}", SectionType::Unhashed), "UNHASHED");
    }

    #[test]
    fn test_section_type_ordering() {
        // Sections must have strictly increasing ordering values
        let ordered = [
            SectionType::Header,
            SectionType::Dependencies,
            SectionType::Provenance,
            SectionType::Graph,
            SectionType::Semantic,
            SectionType::Content,
            SectionType::Unhashed,
        ];

        for i in 1..ordered.len() {
            assert!(
                ordered[i - 1].ordering() < ordered[i].ordering(),
                "{} (ordering {}) should come before {} (ordering {})",
                ordered[i - 1].name(),
                ordered[i - 1].ordering(),
                ordered[i].name(),
                ordered[i].ordering(),
            );
        }
    }

    #[test]
    fn test_section_type_postcard_roundtrip() {
        let types = [
            SectionType::Header,
            SectionType::Dependencies,
            SectionType::Provenance,
            SectionType::Graph,
            SectionType::Content,
            SectionType::Semantic,
            SectionType::Unhashed,
        ];

        for st in &types {
            let bytes = postcard::to_allocvec(st).unwrap();
            let decoded: SectionType = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(*st, decoded);
        }
    }

    // ── FileHeaderFlags ───────────────────────────────────────────

    #[test]
    fn test_flags_none() {
        let flags = FileHeaderFlags::NONE;
        assert!(flags.is_empty());
        assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(!flags.has(FileHeaderFlags::HAS_SEMANTIC));
        assert!(!flags.has(FileHeaderFlags::HAS_UNHASHED));
        assert!(!flags.has_unknown_flags());
    }

    #[test]
    fn test_flags_set_and_clear() {
        let mut flags = FileHeaderFlags::NONE;

        flags.set(FileHeaderFlags::HAS_PROVENANCE);
        assert!(flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(!flags.has(FileHeaderFlags::HAS_SEMANTIC));

        flags.set(FileHeaderFlags::HAS_SEMANTIC);
        assert!(flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(flags.has(FileHeaderFlags::HAS_SEMANTIC));

        flags.clear(FileHeaderFlags::HAS_PROVENANCE);
        assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(flags.has(FileHeaderFlags::HAS_SEMANTIC));
    }

    #[test]
    fn test_flags_raw_roundtrip() {
        let mut flags = FileHeaderFlags::NONE;
        flags.set(FileHeaderFlags::HAS_PROVENANCE);
        flags.set(FileHeaderFlags::HAS_UNHASHED);

        let raw = flags.to_raw();
        let decoded = FileHeaderFlags::from_raw(raw);
        assert_eq!(flags, decoded);
    }

    #[test]
    fn test_flags_unknown_bits() {
        let flags = FileHeaderFlags::from_raw(0xFF00_0000);
        assert!(flags.has_unknown_flags());
        assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
    }

    #[test]
    fn test_flags_display_none() {
        let flags = FileHeaderFlags::NONE;
        assert_eq!(format!("{}", flags), "(none)");
    }

    #[test]
    fn test_flags_display_some() {
        let mut flags = FileHeaderFlags::NONE;
        flags.set(FileHeaderFlags::HAS_PROVENANCE);
        flags.set(FileHeaderFlags::HAS_SEMANTIC);
        let display = format!("{}", flags);
        assert!(display.contains("PROVENANCE"));
        assert!(display.contains("SEMANTIC"));
    }

    // ── FileHeader ────────────────────────────────────────────────

    #[test]
    fn test_file_header_size() {
        assert_eq!(FileHeader::SIZE, 64);
    }

    #[test]
    fn test_file_header_default() {
        let header = FileHeader::default();
        assert_eq!(header.magic, MAGIC);
        assert_eq!(header.version, FORMAT_VERSION);
        assert!(header.flags.is_empty());
        assert_eq!(header.hash_table_entries, 0);
        assert_eq!(header.graph_section_count, 0);
        assert_eq!(header.semantic_section_count, 0);
        assert_eq!(header.contents_chunks, 0);
        assert_eq!(header.total_uncompressed, 0);
        assert_eq!(header.reserved, [0u8; 28]);
    }

    #[test]
    fn test_file_header_to_bytes_magic() {
        let header = FileHeader::default();
        let bytes = header.to_bytes();
        assert_eq!(&bytes[0..4], b"ATOM");
    }

    #[test]
    fn test_file_header_to_bytes_version() {
        let header = FileHeader::default();
        let bytes = header.to_bytes();
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(version, FORMAT_VERSION);
    }

    #[test]
    fn test_file_header_roundtrip() {
        let header = FileHeader::builder()
            .hash_table_entries(10)
            .graph_section_count(5)
            .semantic_section_count(5)
            .contents_chunks(20)
            .total_uncompressed(1024 * 1024)
            .with_provenance()
            .with_unhashed()
            .build();

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), FileHeader::SIZE);

        let decoded = FileHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.magic, header.magic);
        assert_eq!(decoded.version, header.version);
        assert_eq!(decoded.flags, header.flags);
        assert_eq!(decoded.hash_table_entries, header.hash_table_entries);
        assert_eq!(decoded.graph_section_count, header.graph_section_count);
        assert_eq!(
            decoded.semantic_section_count,
            header.semantic_section_count
        );
        assert_eq!(decoded.contents_chunks, header.contents_chunks);
        assert_eq!(decoded.total_uncompressed, header.total_uncompressed);
        assert_eq!(decoded.reserved, header.reserved);
    }

    #[test]
    fn test_file_header_from_bytes_invalid_magic() {
        let mut bytes = FileHeader::default().to_bytes();
        bytes[0] = b'X'; // corrupt magic

        let result = FileHeader::from_bytes(&bytes);
        assert!(result.is_err());
        if let Err(FormatError::InvalidMagic { got }) = result {
            assert_eq!(got[0], b'X');
        } else {
            panic!("expected InvalidMagic error");
        }
    }

    #[test]
    fn test_file_header_from_bytes_wrong_version() {
        let mut bytes = FileHeader::default().to_bytes();
        // Set version to 99
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());

        let result = FileHeader::from_bytes(&bytes);
        assert!(result.is_err());
        if let Err(FormatError::UnsupportedVersion { expected, got }) = result {
            assert_eq!(expected, FORMAT_VERSION);
            assert_eq!(got, 99);
        } else {
            panic!("expected UnsupportedVersion error");
        }
    }

    #[test]
    fn test_file_header_io_roundtrip() {
        let header = FileHeader::builder()
            .hash_table_entries(3)
            .graph_section_count(2)
            .build();

        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), FileHeader::SIZE);

        let mut cursor = std::io::Cursor::new(&buf);
        let decoded = FileHeader::read_from(&mut cursor).unwrap();
        assert_eq!(decoded.hash_table_entries, 3);
        assert_eq!(decoded.graph_section_count, 2);
    }

    #[test]
    fn test_file_header_read_truncated() {
        // Only 10 bytes — not enough for a full header
        let buf = [0u8; 10];
        let mut cursor = std::io::Cursor::new(&buf[..]);
        let result = FileHeader::read_from(&mut cursor);
        assert!(result.is_err());
    }

    // ── FileHeaderBuilder ─────────────────────────────────────────

    #[test]
    fn test_builder_default() {
        let header = FileHeader::builder().build();
        assert_eq!(header.magic, MAGIC);
        assert_eq!(header.version, FORMAT_VERSION);
        assert_eq!(header.hash_table_entries, 0);
        assert_eq!(header.graph_section_count, 0);
        assert_eq!(header.semantic_section_count, 0);
        assert_eq!(header.contents_chunks, 0);
        assert_eq!(header.total_uncompressed, 0);
        assert!(header.flags.is_empty());
    }

    #[test]
    fn test_builder_auto_flags_semantic() {
        let header = FileHeader::builder().semantic_section_count(3).build();
        assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));

        let header = FileHeader::builder().semantic_section_count(0).build();
        assert!(!header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
    }

    #[test]
    fn test_builder_auto_flags_provenance() {
        let header = FileHeader::builder().with_provenance().build();
        assert!(header.flags.has(FileHeaderFlags::HAS_PROVENANCE));

        let header = FileHeader::builder().build();
        assert!(!header.flags.has(FileHeaderFlags::HAS_PROVENANCE));
    }

    #[test]
    fn test_builder_auto_flags_unhashed() {
        let header = FileHeader::builder().with_unhashed().build();
        assert!(header.flags.has(FileHeaderFlags::HAS_UNHASHED));
    }

    #[test]
    fn test_builder_chaining() {
        let header = FileHeader::builder()
            .hash_table_entries(5)
            .graph_section_count(10)
            .semantic_section_count(10)
            .contents_chunks(50)
            .total_uncompressed(5_000_000)
            .with_provenance()
            .with_unhashed()
            .build();

        assert_eq!(header.hash_table_entries, 5);
        assert_eq!(header.graph_section_count, 10);
        assert_eq!(header.semantic_section_count, 10);
        assert_eq!(header.contents_chunks, 50);
        assert_eq!(header.total_uncompressed, 5_000_000);
        assert!(header.flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
        assert!(header.flags.has(FileHeaderFlags::HAS_UNHASHED));
    }

    // ── FileHeader::total_section_count ───────────────────────────

    #[test]
    fn test_total_section_count_minimal() {
        // Minimal: HEADER + DEPS = 2
        let header = FileHeader::default();
        assert_eq!(header.total_section_count(), 2);
    }

    #[test]
    fn test_total_section_count_with_all() {
        let header = FileHeader::builder()
            .graph_section_count(3)
            .semantic_section_count(3)
            .contents_chunks(10)
            .with_provenance()
            .with_unhashed()
            .build();

        // 2 (HEADER + DEPS) + 1 (PROVENANCE) + 3 (GRAPH) + 3 (SEMANTIC) + 10 (CONTENT) + 1 (UNHASHED) = 20
        assert_eq!(header.total_section_count(), 20);
    }

    // ── FileHeader::validate ──────────────────────────────────────

    #[test]
    fn test_validate_default_header() {
        let header = FileHeader::default();
        assert!(header.validate().is_ok());
    }

    #[test]
    fn test_validate_builder_header() {
        let header = FileHeader::builder()
            .hash_table_entries(100)
            .graph_section_count(5)
            .semantic_section_count(5)
            .build();
        assert!(header.validate().is_ok());
    }

    #[test]
    fn test_validate_hash_table_too_large() {
        let mut header = FileHeader::default();
        header.hash_table_entries = (MAX_HASH_TABLE_ENTRIES as u32) + 1;
        assert!(header.validate().is_err());
    }

    #[test]
    fn test_validate_semantic_flag_mismatch_flag_set_count_zero() {
        let mut header = FileHeader::default();
        header.flags.set(FileHeaderFlags::HAS_SEMANTIC);
        header.semantic_section_count = 0;
        assert!(header.validate().is_err());
    }

    #[test]
    fn test_validate_semantic_flag_mismatch_count_set_flag_clear() {
        let mut header = FileHeader::default();
        header.semantic_section_count = 5;
        // Don't set HAS_SEMANTIC flag
        assert!(header.validate().is_err());
    }

    // ── SectionHeader ─────────────────────────────────────────────

    #[test]
    fn test_section_header_size() {
        assert_eq!(SectionHeader::SIZE, 5);
    }

    #[test]
    fn test_section_header_roundtrip() {
        let header = SectionHeader::new(SectionType::Graph, 12345);
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), SectionHeader::SIZE);

        let decoded = SectionHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.section_type, SectionType::Graph);
        assert_eq!(decoded.compressed_len, 12345);
    }

    #[test]
    fn test_section_header_all_types() {
        let types = [
            SectionType::Header,
            SectionType::Dependencies,
            SectionType::Provenance,
            SectionType::Graph,
            SectionType::Content,
            SectionType::Semantic,
            SectionType::Unhashed,
        ];

        for st in &types {
            let header = SectionHeader::new(*st, 999);
            let bytes = header.to_bytes();
            let decoded = SectionHeader::from_bytes(&bytes).unwrap();
            assert_eq!(decoded.section_type, *st);
            assert_eq!(decoded.compressed_len, 999);
        }
    }

    #[test]
    fn test_section_header_io_roundtrip() {
        let header = SectionHeader::new(SectionType::Semantic, 4096);

        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), SectionHeader::SIZE);

        let mut cursor = std::io::Cursor::new(&buf);
        let decoded = SectionHeader::read_from(&mut cursor).unwrap();
        assert_eq!(decoded.section_type, SectionType::Semantic);
        assert_eq!(decoded.compressed_len, 4096);
    }

    #[test]
    fn test_section_header_invalid_type() {
        let bytes: [u8; 5] = [0xFF, 0, 0, 0, 0];
        let result = SectionHeader::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_section_header_max_compressed_len() {
        let header = SectionHeader::new(SectionType::Content, u32::MAX);
        let bytes = header.to_bytes();
        let decoded = SectionHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.compressed_len, u32::MAX);
    }

    // ── ContentChunkHeader ────────────────────────────────────────

    #[test]
    fn test_content_chunk_header_size() {
        assert_eq!(ContentChunkHeader::SIZE, 45);
    }

    #[test]
    fn test_content_chunk_header_roundtrip() {
        let hash = [42u8; 32];
        let header = ContentChunkHeader::new(7, hash, 65536, 32000);

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), ContentChunkHeader::SIZE);
        assert_eq!(bytes[0], SectionType::Content.to_byte());

        let decoded = ContentChunkHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.chunk_index, 7);
        assert_eq!(decoded.chunk_hash, hash);
        assert_eq!(decoded.uncompressed_len, 65536);
        assert_eq!(decoded.compressed_len, 32000);
    }

    #[test]
    fn test_content_chunk_header_io_roundtrip() {
        let hash = blake3::hash(b"test content").as_bytes().to_owned();
        let mut chunk_hash = [0u8; 32];
        chunk_hash.copy_from_slice(&hash);

        let header = ContentChunkHeader::new(0, chunk_hash, 1024, 512);

        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), ContentChunkHeader::SIZE);

        let mut cursor = std::io::Cursor::new(&buf);
        let decoded = ContentChunkHeader::read_from(&mut cursor).unwrap();
        assert_eq!(decoded.chunk_index, 0);
        assert_eq!(decoded.chunk_hash, chunk_hash);
        assert_eq!(decoded.uncompressed_len, 1024);
        assert_eq!(decoded.compressed_len, 512);
    }

    #[test]
    fn test_content_chunk_header_wrong_section_type() {
        let mut bytes = [0u8; ContentChunkHeader::SIZE];
        bytes[0] = SectionType::Graph.to_byte(); // Wrong type!

        let result = ContentChunkHeader::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_content_chunk_header_compression_ratio() {
        let header = ContentChunkHeader::new(0, [0u8; 32], 1000, 500);
        assert!((header.compression_ratio() - 0.5).abs() < f64::EPSILON);

        let header = ContentChunkHeader::new(0, [0u8; 32], 1000, 1000);
        assert!((header.compression_ratio() - 1.0).abs() < f64::EPSILON);

        let header = ContentChunkHeader::new(0, [0u8; 32], 0, 0);
        assert!(header.compression_ratio().is_nan());
    }

    // ── Trailer ───────────────────────────────────────────────────

    #[test]
    fn test_trailer_size() {
        assert_eq!(Trailer::SIZE, 32);
    }

    #[test]
    fn test_trailer_roundtrip() {
        let hash = blake3::hash(b"change content").as_bytes().to_owned();
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(&hash);

        let trailer = Trailer { content_hash };
        let bytes = trailer.to_bytes();
        assert_eq!(bytes.len(), Trailer::SIZE);

        let decoded = Trailer::from_bytes(&bytes);
        assert_eq!(decoded.content_hash, content_hash);
    }

    #[test]
    fn test_trailer_io_roundtrip() {
        let trailer = Trailer {
            content_hash: [0xAB; 32],
        };

        let mut buf = Vec::new();
        trailer.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), Trailer::SIZE);

        let mut cursor = std::io::Cursor::new(&buf);
        let decoded = Trailer::read_from(&mut cursor).unwrap();
        assert_eq!(decoded.content_hash, [0xAB; 32]);
    }

    #[test]
    fn test_trailer_equality() {
        let a = Trailer {
            content_hash: [1; 32],
        };
        let b = Trailer {
            content_hash: [1; 32],
        };
        let c = Trailer {
            content_hash: [2; 32],
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── Cross-type integration ────────────────────────────────────

    #[test]
    fn test_full_header_section_trailer_sizes() {
        // Verify the combined overhead for a minimal change file:
        // FileHeader (64) + SectionHeader*2 (10) + Trailer (32) = 106 bytes overhead minimum
        let overhead = FileHeader::SIZE + (SectionHeader::SIZE * 2) + Trailer::SIZE;
        assert_eq!(overhead, 106);
    }

    #[test]
    fn test_file_header_preserved_in_bytes() {
        // Ensure reserved bytes stay zero through roundtrip
        let header = FileHeader::builder()
            .hash_table_entries(42)
            .graph_section_count(1)
            .build();

        let bytes = header.to_bytes();
        for i in 36..64 {
            assert_eq!(bytes[i], 0, "reserved byte at index {} should be zero", i);
        }
    }
}

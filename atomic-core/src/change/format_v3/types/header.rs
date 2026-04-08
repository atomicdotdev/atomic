//! File header types for V3 change files.
//!
//! [`FileHeaderFlags`] is a bitfield stored in the header's `flags` field.
//! [`FileHeader`] is the fixed 64-byte header at the start of every V3 file.
//! [`FileHeaderBuilder`] provides a fluent API for constructing headers.

use super::super::error::{
    FormatError, FormatResult, FORMAT_VERSION, MAGIC, MAX_HASH_TABLE_ENTRIES,
};
use super::builder::FileHeaderBuilder;
use std::fmt;
use std::io::{Read, Write};

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

//! Section framing types for V3 change files.
//!
//! [`SectionType`] discriminates the different section kinds in a V3 file.
//! [`SectionHeader`] is the 5-byte framing header for regular sections.
//! [`ContentChunkHeader`] is the 45-byte extended header for content chunks.

use super::super::error::{FormatError, FormatResult};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Read, Write};

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

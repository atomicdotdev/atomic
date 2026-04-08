//! Streaming reader for the Change Format V3.
//!
//! The [`ChangeReader`] reads a complete V3 change file from any [`Read`] source,
//! validating the file header, deserializing sections on demand, and verifying
//! the content hash against the trailer.
//!
//! # Design
//!
//! The reader supports three usage patterns:
//!
//! 1. **Sequential reading**: Read all sections in order via [`next_section`](ChangeReader::next_section),
//!    then verify the hash via [`verify`](ChangeReader::verify).
//!
//! 2. **Selective reading**: Peek at section types via [`peek_section_type`](ChangeReader::peek_section_type),
//!    skip unwanted sections via [`skip_section`](ChangeReader::skip_section), and only
//!    decompress the sections you need.
//!
//! 3. **Layer-selective reading**: Use [`graph_sections`](ChangeReader::graph_sections),
//!    [`semantic_sections`](ChangeReader::semantic_sections), or
//!    [`content_chunks`](ChangeReader::content_chunks) to read only a specific layer,
//!    automatically skipping all other section types. The hash still verifies correctly
//!    because skipped sections are fed through the hasher without decompression.
//!
//! # Incremental Hash Verification
//!
//! Like the writer, the reader computes a blake3 hash incrementally as it reads
//! hashed sections. After reading all sections, [`verify`](ChangeReader::verify)
//! compares the computed hash against the trailer. If they don't match, the
//! file is corrupt.
//!
//! The hash covers (in order):
//! 1. Hash dedup table bytes (raw)
//! 2. All section headers + compressed payloads (in file order)
//! 3. Content chunk headers + compressed payloads (in file order)
//!
//! The UNHASHED section and the file header / trailer are excluded.
//!
//! # Memory Usage
//!
//! The reader decompresses one section at a time. Peak memory is proportional
//! to the **largest single section**, not the total change size.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::format_v3::{
//!     ChangeWriter, ChangeReader, HashDedupTable, FileHeader, WriterOptions,
//!     SectionType,
//! };
//! use atomic_core::change::ChangeHeader;
//!
//! // Write a minimal change file
//! let mut buf = Vec::new();
//! let self_hash = *blake3::hash(b"test").as_bytes();
//! let hash_table = HashDedupTable::new(self_hash);
//! let file_header = FileHeader::builder().hash_table_entries(1).build();
//!
//! let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
//! writer.write_file_header(&file_header).unwrap();
//! writer.write_hash_table(&hash_table).unwrap();
//! writer.write_change_header(&ChangeHeader::new("Hello")).unwrap();
//! writer.write_dependencies(&[]).unwrap();
//! let outcome = writer.finalize().unwrap();
//!
//! // Read it back
//! let mut cursor = std::io::Cursor::new(&buf);
//! let mut reader = ChangeReader::open(&mut cursor).unwrap();
//!
//! assert_eq!(reader.file_header().hash_table_entries, 1);
//! assert_eq!(reader.hash_table().len(), 1);
//!
//! // Read sections
//! let section = reader.next_section().unwrap().unwrap();
//! assert_eq!(section.section_type, SectionType::Header);
//!
//! let section = reader.next_section().unwrap().unwrap();
//! assert_eq!(section.section_type, SectionType::Dependencies);
//!
//! // No more sections
//! assert!(reader.next_section().unwrap().is_none());
//!
//! // Verify hash
//! let verified = reader.verify().unwrap();
//! assert_eq!(verified, outcome.content_hash);
//! ```

use super::error::FormatResult;
use super::hash_table::HashDedupTable;
use super::types::{ContentChunkHeader, FileHeader, SectionType};
use std::io::Read;

// ═══════════════════════════════════════════════════════════════════════
// ReadSection — a single decompressed section from the file
// ═══════════════════════════════════════════════════════════════════════

/// A single section read from a V3 change file.
///
/// Contains the section type and its decompressed payload. For most
/// section types the payload is postcard-serialized data that the caller
/// deserializes with [`postcard::from_bytes`]. For CONTENT chunks the
/// payload is raw (uncompressed) file content. For UNHASHED the payload
/// is typically JSON.
///
/// # Deserialization Helpers
///
/// Use [`deserialize`](ReadSection::deserialize) to decode the payload
/// with postcard in one call:
///
/// ```rust,ignore
/// let header: ChangeHeader = section.deserialize()?;
/// ```
///
/// # Content Chunks
///
/// Content sections carry additional metadata in [`content_chunk_info`](ReadSection::content_chunk_info).
#[derive(Clone, Debug)]
pub struct ReadSection {
    /// The type of this section.
    pub section_type: SectionType,

    /// The decompressed section payload.
    ///
    /// For most sections this is postcard-serialized data.
    /// For CONTENT sections this is raw file content bytes.
    /// For UNHASHED this is typically JSON.
    pub payload: Vec<u8>,

    /// The compressed size of this section on disk (before decompression).
    ///
    /// Useful for statistics and progress reporting.
    pub compressed_size: u32,

    /// Additional metadata for CONTENT chunks. `None` for other section types.
    pub content_chunk_info: Option<ContentChunkInfo>,
}

impl ReadSection {
    /// Deserialize the payload from postcard format.
    ///
    /// Convenience wrapper around [`postcard::from_bytes`] that handles
    /// the error conversion.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The target type, must implement `serde::Deserialize`.
    ///
    /// # Errors
    ///
    /// - `FormatError::Postcard` if deserialization fails.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use atomic_core::change::ChangeHeader;
    ///
    /// let header: ChangeHeader = section.deserialize()?;
    /// ```
    pub fn deserialize<'de, T: serde::Deserialize<'de>>(&'de self) -> FormatResult<T> {
        Ok(postcard::from_bytes(&self.payload)?)
    }

    /// Returns `true` if this is a hashed section (contributes to content hash).
    #[inline]
    pub fn is_hashed(&self) -> bool {
        self.section_type.is_hashed()
    }

    /// Returns the uncompressed payload size in bytes.
    #[inline]
    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ContentChunkInfo — additional metadata for CONTENT chunks
// ═══════════════════════════════════════════════════════════════════════

/// Additional metadata carried by CONTENT section chunks.
///
/// This information comes from the [`ContentChunkHeader`] and is exposed
/// here for callers that need to inspect chunk-level details (e.g., for
/// delta transfer or integrity checking).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentChunkInfo {
    /// Sequential chunk index (0-based).
    pub chunk_index: u32,

    /// Blake3 hash of the uncompressed chunk data.
    pub chunk_hash: [u8; 32],

    /// Uncompressed size of the chunk data.
    pub uncompressed_len: u32,
}

// ═══════════════════════════════════════════════════════════════════════
// ReaderStats — statistics collected during reading
// ═══════════════════════════════════════════════════════════════════════

/// Statistics collected during the reading process.
///
/// Available via [`ChangeReader::stats`] at any point during reading.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Number of sections read (decompressed).
    pub sections_read: u32,

    /// Number of sections skipped without decompressing.
    pub sections_skipped: u32,

    /// Number of GRAPH sections read.
    pub graph_sections_read: u32,

    /// Number of SEMANTIC sections read.
    pub semantic_sections_read: u32,

    /// Number of CONTENT chunks read.
    pub content_chunks_read: u32,

    /// Total decompressed bytes across all sections.
    pub total_decompressed: u64,

    /// Total compressed bytes read from the source.
    pub total_compressed: u64,

    /// Total bytes read from the source (including framing).
    pub total_bytes_read: u64,
}

impl std::fmt::Display for ReaderStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sections read, {} skipped, {} bytes compressed → {} bytes decompressed, {} bytes total from disk",
            self.sections_read,
            self.sections_skipped,
            self.total_compressed,
            self.total_decompressed,
            self.total_bytes_read,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ChangeReader — the main reader
// ═══════════════════════════════════════════════════════════════════════

/// Streaming reader for V3 change files.
///
/// Reads and validates a V3 change file from any [`Read`] source. The
/// reader provides section-by-section access with on-demand decompression
/// and incremental hash verification.
///
/// # Opening
///
/// [`ChangeReader::open`] reads the file header and hash dedup table, then
/// returns the reader positioned at the first section. The file header and
/// hash table are validated during opening.
///
/// # Section Access
///
/// Sections are accessed sequentially. The reader maintains the file position
/// and returns sections one at a time:
///
/// - [`next_section`](Self::next_section) — Read and decompress the next section.
/// - [`skip_section`](Self::skip_section) — Skip the next section without decompressing.
/// - [`peek_section_type`](Self::peek_section_type) — Peek at the next section's type
///   without consuming it.
///
/// # Selective Reading
///
/// For "thin pull" or "thin review" scenarios, use [`peek_section_type`](Self::peek_section_type)
/// and [`skip_section`](Self::skip_section) to read only the layers you need:
///
/// ```rust,ignore
/// // Read only GRAPH sections (thin pull)
/// while let Some(section_type) = reader.peek_section_type()? {
///     if section_type == SectionType::Graph {
///         let section = reader.next_section()?.unwrap();
///         apply_graph_ops(&section);
///     } else {
///         reader.skip_section()?;
///     }
/// }
/// ```
///
/// # Hash Verification
///
/// After reading all sections, call [`verify`](Self::verify) to check
/// the content hash against the trailer:
///
/// ```rust,ignore
/// let content_hash = reader.verify()?;
/// // content_hash is the verified blake3 hash of all hashed sections
/// ```
///
/// # Thread Safety
///
/// `ChangeReader` is NOT thread-safe — it wraps a `&mut R` reader.
pub struct ChangeReader<'r, R: Read> {
    /// The underlying reader.
    reader: &'r mut R,

    /// The parsed file header.
    file_header: FileHeader,

    /// The parsed hash dedup table.
    hash_table: HashDedupTable,

    /// Incremental blake3 hasher for verification.
    hasher: blake3::Hasher,

    /// How many more sections we expect (counted down from the file header).
    remaining_sections: u32,

    /// Accumulated statistics.
    stats: ReaderStats,

    /// Peeked section header (from `peek_section_type`), awaiting consumption.
    peeked: Option<PeekedSection>,

    /// Whether we've read all sections and are ready for the trailer.
    sections_exhausted: bool,
}

/// Internal state for a peeked section header.
///
/// When [`peek_section_type`] is called, we read the section header bytes
/// from the reader and stash them here. The next call to [`next_section`]
/// or [`skip_section`] consumes the peeked header without re-reading.
#[derive(Clone, Debug)]
struct PeekedSection {
    /// The raw header bytes — either 5 bytes (SectionHeader) or 45 bytes
    /// (ContentChunkHeader). We store the full bytes so we can re-parse
    /// without re-reading from the source.
    header_bytes: Vec<u8>,

    /// The parsed section type.
    section_type: SectionType,

    /// Compressed payload length (from the header).
    compressed_len: u32,

    /// For CONTENT chunks: the full parsed chunk header.
    chunk_header: Option<ContentChunkHeader>,
}

impl<'r, R: Read> ChangeReader<'r, R> {
    /// Open a V3 change file for reading.
    ///
    /// Reads and validates the 64-byte file header and the hash dedup table.
    /// After this call, the reader is positioned at the first section.
    ///
    /// # Arguments
    ///
    /// * `reader` - The source to read from.
    ///
    /// # Returns
    ///
    /// A `ChangeReader` ready to read sections.
    ///
    /// # Errors
    ///
    /// - `FormatError::InvalidMagic` if the file doesn't start with `b"ATOM"`.
    /// - `FormatError::UnsupportedVersion` if the format version isn't supported.
    /// - `FormatError::InvalidHeader` if the header fields are inconsistent.
    /// - `FormatError::HashTableFull` if the hash table has too many entries.
    /// - I/O errors if the source can't be read.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use atomic_core::change::format_v3::ChangeReader;
    /// use std::io::Cursor;
    ///
    /// let mut cursor = Cursor::new(&change_file_bytes);
    /// let reader = ChangeReader::open(&mut cursor)?;
    /// ```
    pub fn open(reader: &'r mut R) -> FormatResult<Self> {
        let mut hasher = blake3::Hasher::new();
        let mut total_bytes_read: u64 = 0;

        // 1. Read and validate file header (64 bytes)
        let file_header = FileHeader::read_from(reader)?;
        file_header.validate()?;
        total_bytes_read += FileHeader::SIZE as u64;

        // 2. Read hash dedup table
        let hash_table_entry_count = file_header.hash_table_entries;
        let hash_table = HashDedupTable::read_from(reader, hash_table_entry_count)?;
        total_bytes_read += hash_table.serialized_size() as u64;

        // Feed hash table bytes through the hasher (they are part of the content hash)
        let mut hash_table_buf = Vec::with_capacity(hash_table.serialized_size());
        hash_table.write_to(&mut hash_table_buf)?;
        hasher.update(&hash_table_buf);

        // 3. Compute expected total section count
        let remaining_sections = file_header.total_section_count();

        Ok(Self {
            reader,
            file_header,
            hash_table,
            hasher,
            remaining_sections,
            stats: ReaderStats {
                total_bytes_read,
                ..Default::default()
            },
            peeked: None,
            sections_exhausted: false,
        })
    }

    /// Returns a reference to the file header.
    ///
    /// The file header is read and validated during [`open`](Self::open).
    #[inline]
    pub fn file_header(&self) -> &FileHeader {
        &self.file_header
    }

    /// Returns a reference to the hash dedup table.
    ///
    /// The hash table is read during [`open`](Self::open) and provides
    /// bidirectional lookup between hash indices and full 32-byte hashes.
    #[inline]
    pub fn hash_table(&self) -> &HashDedupTable {
        &self.hash_table
    }

    /// Returns the current reader statistics.
    #[inline]
    pub fn stats(&self) -> &ReaderStats {
        &self.stats
    }

    /// Returns the number of remaining sections that haven't been read or skipped.
    ///
    /// This is based on the counts in the file header. It decreases by 1
    /// for each call to [`next_section`](Self::next_section) or
    /// [`skip_section`](Self::skip_section).
    #[inline]
    pub fn remaining_sections(&self) -> u32 {
        self.remaining_sections
    }

    // ── Section Reading ────────────────────────────────────────────
}

mod sections;

#[cfg(test)]
mod tests;

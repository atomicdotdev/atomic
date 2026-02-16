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

use super::error::{FormatError, FormatResult};
use super::hash_table::HashDedupTable;
use super::types::{ContentChunkHeader, FileHeader, SectionHeader, SectionType, Trailer};
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
    /// - [`FormatError::Postcard`] if deserialization fails.
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
    /// - [`FormatError::InvalidMagic`] if the file doesn't start with `b"ATOM"`.
    /// - [`FormatError::UnsupportedVersion`] if the format version isn't supported.
    /// - [`FormatError::InvalidHeader`] if the header fields are inconsistent.
    /// - [`FormatError::HashTableFull`] if the hash table has too many entries.
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

    /// Peek at the next section's type without consuming it.
    ///
    /// Reads the section header from the source and stashes it internally.
    /// The next call to [`next_section`](Self::next_section) or
    /// [`skip_section`](Self::skip_section) will use the peeked header
    /// without re-reading from the source.
    ///
    /// Returns `None` if all sections have been read.
    ///
    /// # Errors
    ///
    /// - [`FormatError::InvalidSectionType`] if the section type byte is unknown.
    /// - I/O errors from the underlying reader.
    pub fn peek_section_type(&mut self) -> FormatResult<Option<SectionType>> {
        if self.sections_exhausted || self.remaining_sections == 0 {
            return Ok(None);
        }

        // If we already peeked, return the cached type
        if let Some(ref peeked) = self.peeked {
            return Ok(Some(peeked.section_type));
        }

        // Read the first byte to determine section type
        let mut type_byte = [0u8; 1];
        match self.reader.read_exact(&mut type_byte) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.sections_exhausted = true;
                return Ok(None);
            }
            Err(e) => return Err(FormatError::Io(e)),
        }

        let section_type = SectionType::from_byte(type_byte[0])?;

        // Read the rest of the header based on type
        let peeked = if section_type == SectionType::Content {
            // Content chunks have an extended 45-byte header.
            // We already read 1 byte (the type), read the remaining 44.
            let mut rest = [0u8; ContentChunkHeader::SIZE - 1];
            self.reader.read_exact(&mut rest)?;

            let mut full_header_bytes = Vec::with_capacity(ContentChunkHeader::SIZE);
            full_header_bytes.push(type_byte[0]);
            full_header_bytes.extend_from_slice(&rest);

            let mut header_arr = [0u8; ContentChunkHeader::SIZE];
            header_arr.copy_from_slice(&full_header_bytes);
            let chunk_header = ContentChunkHeader::from_bytes(&header_arr)?;

            PeekedSection {
                header_bytes: full_header_bytes,
                section_type,
                compressed_len: chunk_header.compressed_len,
                chunk_header: Some(chunk_header),
            }
        } else {
            // Standard sections have a 5-byte header.
            // We already read 1 byte (the type), read the remaining 4.
            let mut rest = [0u8; SectionHeader::SIZE - 1];
            self.reader.read_exact(&mut rest)?;

            let mut full_header_bytes = Vec::with_capacity(SectionHeader::SIZE);
            full_header_bytes.push(type_byte[0]);
            full_header_bytes.extend_from_slice(&rest);

            let mut header_arr = [0u8; SectionHeader::SIZE];
            header_arr.copy_from_slice(&full_header_bytes);
            let section_header = SectionHeader::from_bytes(&header_arr)?;

            PeekedSection {
                header_bytes: full_header_bytes,
                section_type,
                compressed_len: section_header.compressed_len,
                chunk_header: None,
            }
        };

        self.peeked = Some(peeked);
        Ok(Some(section_type))
    }

    /// Read the next section, decompressing its payload.
    ///
    /// Returns `None` if all sections have been read. The returned
    /// [`ReadSection`] contains the decompressed payload and metadata.
    ///
    /// The compressed bytes of hashed sections are fed through the
    /// blake3 hasher for later verification.
    ///
    /// # Errors
    ///
    /// - [`FormatError::InvalidSectionType`] if the section type is unknown.
    /// - [`FormatError::Decompress`] if zstd decompression fails.
    /// - I/O errors from the underlying reader.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// while let Some(section) = reader.next_section()? {
    ///     println!("{}: {} bytes", section.section_type, section.payload_len());
    /// }
    /// ```
    pub fn next_section(&mut self) -> FormatResult<Option<ReadSection>> {
        // Ensure we have a peeked header (reads from source if needed)
        if self.peeked.is_none() {
            if self.peek_section_type()?.is_none() {
                return Ok(None);
            }
        }

        let peeked = self.peeked.take().unwrap();

        // Read the compressed payload
        let mut compressed = vec![0u8; peeked.compressed_len as usize];
        self.reader.read_exact(&mut compressed)?;

        // Feed through hasher if this is a hashed section
        if peeked.section_type.is_hashed() {
            self.hasher.update(&peeked.header_bytes);
            self.hasher.update(&compressed);
        }

        // Decompress
        let decompressed = if peeked.compressed_len == 0 {
            Vec::new()
        } else {
            zstd::decode_all(&compressed[..]).map_err(|e| FormatError::Decompress(e.to_string()))?
        };

        // Build content chunk info if applicable
        let content_chunk_info = peeked.chunk_header.map(|ch| ContentChunkInfo {
            chunk_index: ch.chunk_index,
            chunk_hash: ch.chunk_hash,
            uncompressed_len: ch.uncompressed_len,
        });

        // Update stats
        let header_size = peeked.header_bytes.len() as u64;
        self.stats.sections_read += 1;
        self.stats.total_compressed += peeked.compressed_len as u64;
        self.stats.total_decompressed += decompressed.len() as u64;
        self.stats.total_bytes_read += header_size + peeked.compressed_len as u64;
        self.remaining_sections = self.remaining_sections.saturating_sub(1);

        match peeked.section_type {
            SectionType::Graph => self.stats.graph_sections_read += 1,
            SectionType::Semantic => self.stats.semantic_sections_read += 1,
            SectionType::Content => self.stats.content_chunks_read += 1,
            _ => {}
        }

        if self.remaining_sections == 0 {
            self.sections_exhausted = true;
        }

        Ok(Some(ReadSection {
            section_type: peeked.section_type,
            payload: decompressed,
            compressed_size: peeked.compressed_len,
            content_chunk_info,
        }))
    }

    /// Skip the next section without decompressing it.
    ///
    /// Reads the section header and compressed payload from the source
    /// but does not decompress. The compressed bytes of hashed sections
    /// are still fed through the blake3 hasher so that verification
    /// works correctly even when sections are skipped.
    ///
    /// Returns `Ok(false)` if there are no more sections to skip.
    /// Returns `Ok(true)` if a section was successfully skipped.
    ///
    /// # Errors
    ///
    /// - [`FormatError::InvalidSectionType`] if the section type is unknown.
    /// - I/O errors from the underlying reader.
    pub fn skip_section(&mut self) -> FormatResult<bool> {
        // Ensure we have a peeked header
        if self.peeked.is_none() {
            if self.peek_section_type()?.is_none() {
                return Ok(false);
            }
        }

        let peeked = self.peeked.take().unwrap();

        // Read (but don't decompress) the compressed payload
        let mut compressed = vec![0u8; peeked.compressed_len as usize];
        self.reader.read_exact(&mut compressed)?;

        // Feed through hasher if hashed (required for correct verification)
        if peeked.section_type.is_hashed() {
            self.hasher.update(&peeked.header_bytes);
            self.hasher.update(&compressed);
        }

        // Update stats
        let header_size = peeked.header_bytes.len() as u64;
        self.stats.sections_skipped += 1;
        self.stats.total_compressed += peeked.compressed_len as u64;
        self.stats.total_bytes_read += header_size + peeked.compressed_len as u64;
        self.remaining_sections = self.remaining_sections.saturating_sub(1);

        if self.remaining_sections == 0 {
            self.sections_exhausted = true;
        }

        Ok(true)
    }

    /// Read all remaining sections, returning them as a vector.
    ///
    /// This is a convenience method that calls [`next_section`](Self::next_section)
    /// in a loop until all sections are consumed.
    ///
    /// # Errors
    ///
    /// Any error from [`next_section`](Self::next_section).
    pub fn read_all_sections(&mut self) -> FormatResult<Vec<ReadSection>> {
        let mut sections = Vec::new();
        while let Some(section) = self.next_section()? {
            sections.push(section);
        }
        Ok(sections)
    }

    // ── Verification ───────────────────────────────────────────────

    /// Verify the content hash by reading the trailer.
    ///
    /// Reads the 32-byte trailer from the source and compares it to the
    /// blake3 hash computed incrementally from all hashed sections.
    ///
    /// This should be called **after** all sections have been read or skipped.
    /// Sections that were skipped still contribute to the hash (their
    /// compressed bytes are fed through the hasher during skipping).
    ///
    /// # Returns
    ///
    /// The verified 32-byte content hash on success.
    ///
    /// # Errors
    ///
    /// - [`FormatError::HashMismatch`] if the computed hash doesn't match the trailer.
    /// - I/O errors from reading the trailer.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// // Read all sections first
    /// let sections = reader.read_all_sections()?;
    ///
    /// // Then verify
    /// let content_hash = reader.verify()?;
    /// println!("Verified content hash: {:02x?}", &content_hash[..8]);
    /// ```
    pub fn verify(&mut self) -> FormatResult<[u8; 32]> {
        // Read the trailer
        let trailer = Trailer::read_from(self.reader)?;
        self.stats.total_bytes_read += Trailer::SIZE as u64;

        // Compute the final hash from everything we've fed through the hasher
        let computed = self.hasher.finalize();
        let computed_bytes = *computed.as_bytes();

        // Compare
        if computed_bytes != trailer.content_hash {
            return Err(FormatError::HashMismatch {
                expected: format!("{:02x?}", &trailer.content_hash[..8]),
                computed: format!("{:02x?}", &computed_bytes[..8]),
            });
        }

        Ok(computed_bytes)
    }

    /// Read all remaining sections and verify the content hash in one call.
    ///
    /// This is a convenience method that combines [`read_all_sections`](Self::read_all_sections)
    /// and [`verify`](Self::verify).
    ///
    /// # Returns
    ///
    /// A tuple of `(sections, content_hash)`.
    ///
    /// # Errors
    ///
    /// Any error from section reading or hash verification.
    pub fn read_all_and_verify(&mut self) -> FormatResult<(Vec<ReadSection>, [u8; 32])> {
        let sections = self.read_all_sections()?;
        let hash = self.verify()?;
        Ok((sections, hash))
    }

    // ── Layer-Selective Reading ─────────────────────────────────────

    /// Read only GRAPH sections, skipping everything else.
    ///
    /// This is the "thin pull" pattern: read only the storage/merge layer
    /// needed to apply a change, without downloading or decompressing the
    /// semantic layer or content chunks.
    ///
    /// All skipped sections are still fed through the blake3 hasher so that
    /// [`verify`](Self::verify) works correctly after this call.
    ///
    /// # Returns
    ///
    /// A vector of [`ReadSection`] where every entry has
    /// `section_type == SectionType::Graph`.
    ///
    /// # Errors
    ///
    /// Any error from section reading or skipping.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let graph_sections = reader.graph_sections()?;
    /// for section in &graph_sections {
    ///     let payload = GraphSectionPayload::from_postcard_bytes(&section.payload)?;
    ///     apply_graph_ops(&payload);
    /// }
    /// let hash = reader.verify()?; // still works
    /// ```
    pub fn graph_sections(&mut self) -> FormatResult<Vec<ReadSection>> {
        self.sections_of_type(SectionType::Graph)
    }

    /// Read only SEMANTIC sections, skipping everything else.
    ///
    /// This is the "thin review" pattern: read only the display/analysis
    /// layer for code review, diff display, and blame — without loading
    /// graph operations or content chunks.
    ///
    /// All skipped sections are still fed through the blake3 hasher so that
    /// [`verify`](Self::verify) works correctly after this call.
    ///
    /// # Returns
    ///
    /// A vector of [`ReadSection`] where every entry has
    /// `section_type == SectionType::Semantic`.
    ///
    /// # Errors
    ///
    /// Any error from section reading or skipping.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let semantic_sections = reader.semantic_sections()?;
    /// for section in &semantic_sections {
    ///     let ops: Vec<FileOps> = postcard::from_bytes(&section.payload)?;
    ///     display_diff(&ops);
    /// }
    /// ```
    pub fn semantic_sections(&mut self) -> FormatResult<Vec<ReadSection>> {
        self.sections_of_type(SectionType::Semantic)
    }

    /// Read only CONTENT sections, skipping everything else.
    ///
    /// This reads the raw file content chunks without loading graph or
    /// semantic operations. Useful when combined with semantic sections
    /// for code review (semantic + content = full diff display without
    /// needing graph operations).
    ///
    /// Each returned section has `content_chunk_info` populated with the
    /// chunk index, blake3 hash, and uncompressed size.
    ///
    /// All skipped sections are still fed through the blake3 hasher so that
    /// [`verify`](Self::verify) works correctly after this call.
    ///
    /// # Returns
    ///
    /// A vector of [`ReadSection`] where every entry has
    /// `section_type == SectionType::Content`.
    ///
    /// # Errors
    ///
    /// Any error from section reading or skipping.
    pub fn content_chunks(&mut self) -> FormatResult<Vec<ReadSection>> {
        self.sections_of_type(SectionType::Content)
    }

    /// Internal: read only sections of a specific type, skipping all others.
    ///
    /// Skipped sections are still fed through the hasher for verification.
    fn sections_of_type(&mut self, target: SectionType) -> FormatResult<Vec<ReadSection>> {
        let mut result = Vec::new();

        loop {
            match self.peek_section_type()? {
                Some(st) if st == target => {
                    if let Some(section) = self.next_section()? {
                        result.push(section);
                    }
                }
                Some(_) => {
                    self.skip_section()?;
                }
                None => break,
            }
        }

        Ok(result)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::format_v3::{ChangeWriter, FileHeader, WriterOptions};
    use crate::change::header::ChangeHeader;
    use std::io::Cursor;

    /// Helper: write a minimal valid change file and return (bytes, content_hash).
    fn write_minimal_change() -> (Vec<u8>, [u8; 32]) {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"minimal").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);

        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(0)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Minimal change"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        let outcome = writer.finalize().unwrap();

        (buf, outcome.content_hash)
    }

    // ── Layer-Selective Reading ─────────────────────────────────────

    #[test]
    fn test_graph_sections_convenience() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let graph = reader.graph_sections().unwrap();

        assert_eq!(graph.len(), 2);
        for s in &graph {
            assert_eq!(s.section_type, SectionType::Graph);
        }

        // Hash should still verify after selective read
        let hash = reader.verify().unwrap();
        assert_eq!(hash, expected_hash);
    }

    #[test]
    fn test_semantic_sections_convenience() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let semantic = reader.semantic_sections().unwrap();

        assert_eq!(semantic.len(), 2);
        for s in &semantic {
            assert_eq!(s.section_type, SectionType::Semantic);
        }

        let hash = reader.verify().unwrap();
        assert_eq!(hash, expected_hash);
    }

    #[test]
    fn test_content_chunks_convenience() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let content = reader.content_chunks().unwrap();

        assert_eq!(content.len(), 3);
        for s in &content {
            assert_eq!(s.section_type, SectionType::Content);
            assert!(s.content_chunk_info.is_some());
        }

        let hash = reader.verify().unwrap();
        assert_eq!(hash, expected_hash);
    }

    #[test]
    fn test_graph_sections_when_none_exist() {
        // Minimal change has no GRAPH sections
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let graph = reader.graph_sections().unwrap();
        assert!(graph.is_empty());
    }

    #[test]
    fn test_semantic_sections_when_none_exist() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let semantic = reader.semantic_sections().unwrap();
        assert!(semantic.is_empty());
    }

    #[test]
    fn test_content_chunks_when_none_exist() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let content = reader.content_chunks().unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_graph_sections_stats_tracking() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.graph_sections().unwrap();

        let stats = reader.stats();
        assert_eq!(stats.graph_sections_read, 2);
        // All non-GRAPH sections were skipped
        assert!(stats.sections_skipped > 0);
    }

    /// Helper: write a full change file with all section types.
    fn write_full_change() -> (Vec<u8>, [u8; 32]) {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"full").as_bytes();
        let dep_hash = *blake3::hash(b"dep1").as_bytes();

        let mut hash_table = HashDedupTable::new(self_hash);
        let dep_idx = hash_table.insert(dep_hash).unwrap();

        let file_header = FileHeader::builder()
            .hash_table_entries(hash_table.len() as u32)
            .graph_section_count(2)
            .semantic_section_count(2)
            .contents_chunks(3)
            .with_provenance()
            .with_unhashed()
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Full change"))
            .unwrap();
        writer.write_dependencies(&[dep_idx]).unwrap();
        writer.write_provenance(&[]).unwrap();
        writer
            .write_graph_section(b"graph data for file_a.rs")
            .unwrap();
        writer
            .write_graph_section(b"graph data for file_b.rs")
            .unwrap();
        writer
            .write_semantic_section(b"semantic data for file_a.rs")
            .unwrap();
        writer
            .write_semantic_section(b"semantic data for file_b.rs")
            .unwrap();
        writer
            .write_content_chunk(0, b"chunk zero content bytes")
            .unwrap();
        writer
            .write_content_chunk(1, b"chunk one content bytes")
            .unwrap();
        writer
            .write_content_chunk(2, b"chunk two content bytes")
            .unwrap();
        writer
            .write_unhashed(b"{\"transcript\": \"AI reasoning\"}")
            .unwrap();
        let outcome = writer.finalize().unwrap();

        (buf, outcome.content_hash)
    }

    // ── Opening ────────────────────────────────────────────────────

    #[test]
    fn test_open_minimal() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);

        let reader = ChangeReader::open(&mut cursor).unwrap();

        assert_eq!(reader.file_header().hash_table_entries, 1);
        assert_eq!(reader.hash_table().len(), 1);
        assert_eq!(reader.remaining_sections(), 2); // HEADER + DEPS
    }

    #[test]
    fn test_open_full() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);

        let reader = ChangeReader::open(&mut cursor).unwrap();

        assert_eq!(reader.file_header().hash_table_entries, 2);
        assert_eq!(reader.hash_table().len(), 2);
        // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
        assert_eq!(reader.remaining_sections(), 11);
    }

    #[test]
    fn test_open_invalid_magic() {
        let mut buf = vec![0u8; 128];
        buf[0..4].copy_from_slice(b"NOPE");

        let mut cursor = Cursor::new(&buf);
        let result = ChangeReader::open(&mut cursor);

        assert!(result.is_err());
        assert!(matches!(result, Err(FormatError::InvalidMagic { .. })));
    }

    #[test]
    fn test_open_truncated() {
        let buf = vec![0u8; 10]; // way too short
        let mut cursor = Cursor::new(&buf);

        let result = ChangeReader::open(&mut cursor);
        assert!(result.is_err());
    }

    // ── Reading Sections ───────────────────────────────────────────

    #[test]
    fn test_read_minimal_sections() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // First section: HEADER
        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Header);
        assert!(!section.payload.is_empty());
        assert!(section.content_chunk_info.is_none());
        assert!(section.is_hashed());

        // Second section: DEPS
        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Dependencies);
        assert!(section.is_hashed());

        // No more sections
        let section = reader.next_section().unwrap();
        assert!(section.is_none());
    }

    #[test]
    fn test_read_full_sections() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();

        // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
        assert_eq!(sections.len(), 11);

        assert_eq!(sections[0].section_type, SectionType::Header);
        assert_eq!(sections[1].section_type, SectionType::Dependencies);
        assert_eq!(sections[2].section_type, SectionType::Provenance);
        assert_eq!(sections[3].section_type, SectionType::Graph);
        assert_eq!(sections[4].section_type, SectionType::Graph);
        assert_eq!(sections[5].section_type, SectionType::Semantic);
        assert_eq!(sections[6].section_type, SectionType::Semantic);
        assert_eq!(sections[7].section_type, SectionType::Content);
        assert_eq!(sections[8].section_type, SectionType::Content);
        assert_eq!(sections[9].section_type, SectionType::Content);
        assert_eq!(sections[10].section_type, SectionType::Unhashed);
    }

    #[test]
    fn test_read_content_chunk_info() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();

        // Content chunks should have chunk info
        let content_sections: Vec<_> = sections
            .iter()
            .filter(|s| s.section_type == SectionType::Content)
            .collect();

        assert_eq!(content_sections.len(), 3);

        for (i, section) in content_sections.iter().enumerate() {
            let info = section.content_chunk_info.as_ref().unwrap();
            assert_eq!(info.chunk_index, i as u32);
            // The chunk hash should be blake3 of the decompressed content
            let expected_hash = blake3::hash(&section.payload);
            assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
        }
    }

    #[test]
    fn test_read_unhashed_not_hashed() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();
        let unhashed = sections.last().unwrap();
        assert_eq!(unhashed.section_type, SectionType::Unhashed);
        assert!(!unhashed.is_hashed());
    }

    // ── Deserialization ────────────────────────────────────────────

    #[test]
    fn test_deserialize_change_header() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Header);

        let header: ChangeHeader = section.deserialize().unwrap();
        assert_eq!(header.message, "Minimal change");
    }

    #[test]
    fn test_deserialize_dependencies() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Skip HEADER
        reader.next_section().unwrap();

        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Dependencies);

        let deps: Vec<u16> = section.deserialize().unwrap();
        assert!(deps.is_empty()); // no dependencies
    }

    #[test]
    fn test_deserialize_dependencies_with_values() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Skip HEADER
        reader.next_section().unwrap();

        let section = reader.next_section().unwrap().unwrap();
        let deps: Vec<u16> = section.deserialize().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], 1); // dep_idx was 1
    }

    // ── Peek ───────────────────────────────────────────────────────

    #[test]
    fn test_peek_section_type() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Peek without consuming
        let peeked = reader.peek_section_type().unwrap();
        assert_eq!(peeked, Some(SectionType::Header));

        // Peek again — should return the same thing
        let peeked2 = reader.peek_section_type().unwrap();
        assert_eq!(peeked2, Some(SectionType::Header));

        // Now read it — should consume the peeked section
        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Header);

        // Peek the next one
        let peeked3 = reader.peek_section_type().unwrap();
        assert_eq!(peeked3, Some(SectionType::Dependencies));
    }

    #[test]
    fn test_peek_after_all_sections_returns_none() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();

        let peeked = reader.peek_section_type().unwrap();
        assert_eq!(peeked, None);
    }

    // ── Skip ───────────────────────────────────────────────────────

    #[test]
    fn test_skip_section() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Skip HEADER
        assert!(reader.skip_section().unwrap());
        assert_eq!(reader.stats().sections_skipped, 1);

        // Read DEPS normally
        let section = reader.next_section().unwrap().unwrap();
        assert_eq!(section.section_type, SectionType::Dependencies);

        // No more to skip
        assert!(!reader.skip_section().unwrap());
    }

    #[test]
    fn test_skip_all_sections() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let mut skip_count = 0;
        while reader.skip_section().unwrap() {
            skip_count += 1;
        }

        assert_eq!(skip_count, 11); // all sections skipped
        assert_eq!(reader.stats().sections_skipped, 11);
        assert_eq!(reader.stats().sections_read, 0);
    }

    #[test]
    fn test_selective_reading_graph_only() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let mut graph_payloads = Vec::new();

        // Read only GRAPH sections, skip everything else
        loop {
            match reader.peek_section_type().unwrap() {
                Some(SectionType::Graph) => {
                    let section = reader.next_section().unwrap().unwrap();
                    graph_payloads.push(section.payload);
                }
                Some(_) => {
                    reader.skip_section().unwrap();
                }
                None => break,
            }
        }

        assert_eq!(graph_payloads.len(), 2);
        assert_eq!(reader.stats().graph_sections_read, 2);
        assert_eq!(reader.stats().sections_read, 2);
        assert_eq!(reader.stats().sections_skipped, 9); // 11 total - 2 read
    }

    // ── Verification ───────────────────────────────────────────────

    #[test]
    fn test_verify_minimal() {
        let (buf, expected_hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();
        let computed_hash = reader.verify().unwrap();

        assert_eq!(computed_hash, expected_hash);
    }

    #[test]
    fn test_verify_full() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();
        let computed_hash = reader.verify().unwrap();

        assert_eq!(computed_hash, expected_hash);
    }

    #[test]
    fn test_verify_after_skipping_all() {
        // Hash should still verify even when all sections are skipped
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        while reader.skip_section().unwrap() {}
        let computed_hash = reader.verify().unwrap();

        assert_eq!(computed_hash, expected_hash);
    }

    #[test]
    fn test_verify_after_selective_read() {
        // Hash should verify even when some sections are read and some skipped
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Read first two sections, skip the rest
        reader.next_section().unwrap();
        reader.next_section().unwrap();
        while reader.skip_section().unwrap() {}

        let computed_hash = reader.verify().unwrap();
        assert_eq!(computed_hash, expected_hash);
    }

    #[test]
    fn test_verify_corrupt_data() {
        let (mut buf, _hash) = write_minimal_change();

        // Corrupt some bytes in the middle of the file (after the file header)
        if buf.len() > 100 {
            buf[80] ^= 0xFF;
            buf[81] ^= 0xFF;
        }

        let mut cursor = Cursor::new(&buf);
        // Opening might succeed (header and hash table might be fine)
        // but reading + verifying should fail
        let reader_result = ChangeReader::open(&mut cursor);
        if let Ok(mut reader) = reader_result {
            // Try to read all sections — might fail on decompression
            let read_result = reader.read_all_sections();
            if read_result.is_ok() {
                // If sections somehow read, verification should fail
                let verify_result = reader.verify();
                assert!(
                    verify_result.is_err(),
                    "verification should fail on corrupt data"
                );
            }
            // If reading failed, that's also acceptable — corrupt data was detected
        }
        // If opening failed, that's also acceptable
    }

    // ── read_all_and_verify ────────────────────────────────────────

    #[test]
    fn test_read_all_and_verify_minimal() {
        let (buf, expected_hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let (sections, hash) = reader.read_all_and_verify().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(hash, expected_hash);
    }

    #[test]
    fn test_read_all_and_verify_full() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let (sections, hash) = reader.read_all_and_verify().unwrap();
        assert_eq!(sections.len(), 11);
        assert_eq!(hash, expected_hash);
    }

    // ── Stats ──────────────────────────────────────────────────────

    #[test]
    fn test_stats_after_reading() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();

        let stats = reader.stats();
        assert_eq!(stats.sections_read, 11);
        assert_eq!(stats.sections_skipped, 0);
        assert_eq!(stats.graph_sections_read, 2);
        assert_eq!(stats.semantic_sections_read, 2);
        assert_eq!(stats.content_chunks_read, 3);
        assert!(stats.total_compressed > 0);
        assert!(stats.total_decompressed > 0);
        assert!(stats.total_bytes_read > 0);
    }

    #[test]
    fn test_stats_display() {
        let stats = ReaderStats {
            sections_read: 5,
            sections_skipped: 3,
            graph_sections_read: 2,
            semantic_sections_read: 1,
            content_chunks_read: 2,
            total_decompressed: 10000,
            total_compressed: 3000,
            total_bytes_read: 4000,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 sections read"));
        assert!(display.contains("3 skipped"));
        assert!(display.contains("3000 bytes compressed"));
        assert!(display.contains("10000 bytes decompressed"));
    }

    // ── Remaining Sections Counter ─────────────────────────────────

    #[test]
    fn test_remaining_sections_decreases() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        assert_eq!(reader.remaining_sections(), 2);

        reader.next_section().unwrap();
        assert_eq!(reader.remaining_sections(), 1);

        reader.next_section().unwrap();
        assert_eq!(reader.remaining_sections(), 0);
    }

    #[test]
    fn test_remaining_sections_with_skip() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let initial = reader.remaining_sections();
        assert_eq!(initial, 11);

        reader.skip_section().unwrap();
        assert_eq!(reader.remaining_sections(), 10);

        reader.next_section().unwrap();
        assert_eq!(reader.remaining_sections(), 9);
    }

    // ── ReadSection helpers ────────────────────────────────────────

    #[test]
    fn test_read_section_payload_len() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let section = reader.next_section().unwrap().unwrap();
        assert!(section.payload_len() > 0);
        assert_eq!(section.payload_len(), section.payload.len());
    }

    #[test]
    fn test_read_section_is_hashed() {
        let (buf, _hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();

        // All sections except UNHASHED should be hashed
        for section in &sections {
            if section.section_type == SectionType::Unhashed {
                assert!(!section.is_hashed());
            } else {
                assert!(
                    section.is_hashed(),
                    "{:?} should be hashed",
                    section.section_type
                );
            }
        }
    }

    // ── End-to-End Roundtrip ───────────────────────────────────────

    #[test]
    fn test_roundtrip_change_header_content() {
        let original_message = "This is a test change with special chars: 日本語 🚀 <>&";

        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"roundtrip").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder().hash_table_entries(1).build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new(original_message))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        let write_outcome = writer.finalize().unwrap();

        // Read back
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let section = reader.next_section().unwrap().unwrap();
        let header: ChangeHeader = section.deserialize().unwrap();
        assert_eq!(header.message, original_message);

        // Skip DEPS
        reader.skip_section().unwrap();

        let verified_hash = reader.verify().unwrap();
        assert_eq!(verified_hash, write_outcome.content_hash);
    }

    #[test]
    fn test_roundtrip_content_chunks() {
        let chunk_data = vec![
            b"The quick brown fox".to_vec(),
            b"jumps over".to_vec(),
            b"the lazy dog".to_vec(),
        ];

        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"chunks").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .contents_chunks(chunk_data.len() as u32)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Chunks"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        for (i, data) in chunk_data.iter().enumerate() {
            writer.write_content_chunk(i as u32, data).unwrap();
        }
        let write_outcome = writer.finalize().unwrap();

        // Read back
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();

        let content_sections: Vec<_> = sections
            .iter()
            .filter(|s| s.section_type == SectionType::Content)
            .collect();

        assert_eq!(content_sections.len(), 3);

        for (i, section) in content_sections.iter().enumerate() {
            assert_eq!(section.payload, chunk_data[i]);
            let info = section.content_chunk_info.as_ref().unwrap();
            assert_eq!(info.chunk_index, i as u32);
            let expected_hash = blake3::hash(&chunk_data[i]);
            assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
        }

        let verified_hash = reader.verify().unwrap();
        assert_eq!(verified_hash, write_outcome.content_hash);
    }

    #[test]
    fn test_roundtrip_hash_table_resolution() {
        let self_hash = *blake3::hash(b"self").as_bytes();
        let dep1_hash = *blake3::hash(b"dep1").as_bytes();
        let dep2_hash = *blake3::hash(b"dep2").as_bytes();

        let mut hash_table_write = HashDedupTable::new(self_hash);
        hash_table_write.insert(dep1_hash).unwrap();
        hash_table_write.insert(dep2_hash).unwrap();

        let mut buf = Vec::new();
        let file_header = FileHeader::builder()
            .hash_table_entries(hash_table_write.len() as u32)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table_write).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Deps"))
            .unwrap();
        writer.write_dependencies(&[1, 2]).unwrap();
        writer.finalize().unwrap();

        // Read back and verify hash table
        let mut cursor = Cursor::new(&buf);
        let reader = ChangeReader::open(&mut cursor).unwrap();

        let ht = reader.hash_table();
        assert_eq!(ht.len(), 3);
        assert_eq!(ht.resolve(0).unwrap(), &self_hash);
        assert_eq!(ht.resolve(1).unwrap(), &dep1_hash);
        assert_eq!(ht.resolve(2).unwrap(), &dep2_hash);
    }

    // ── Multiple Reads at Different Compression Levels ─────────────

    #[test]
    fn test_different_compression_levels_same_hash() {
        // The hash should be deterministic for the SAME compression level,
        // but DIFFERENT compression levels produce different compressed bytes
        // and therefore different hashes. This is expected behavior.
        //
        // This test verifies that reading works at multiple compression levels.
        for level in [1, 3, 10] {
            let mut buf = Vec::new();
            let self_hash = *blake3::hash(b"level-test").as_bytes();
            let hash_table = HashDedupTable::new(self_hash);
            let file_header = FileHeader::builder()
                .hash_table_entries(1)
                .graph_section_count(1)
                .build();

            let opts = WriterOptions::with_compression_level(level);
            let mut writer = ChangeWriter::new(&mut buf, opts);
            writer.write_file_header(&file_header).unwrap();
            writer.write_hash_table(&hash_table).unwrap();
            writer
                .write_change_header(&ChangeHeader::new("Level test"))
                .unwrap();
            writer.write_dependencies(&[]).unwrap();
            writer.write_graph_section(b"graph data here").unwrap();
            let write_outcome = writer.finalize().unwrap();

            // Read back and verify
            let mut cursor = Cursor::new(&buf);
            let mut reader = ChangeReader::open(&mut cursor).unwrap();
            let (sections, hash) = reader.read_all_and_verify().unwrap();
            assert_eq!(sections.len(), 3); // HEADER + DEPS + GRAPH
            assert_eq!(hash, write_outcome.content_hash);
        }
    }

    // ── Edge Cases ─────────────────────────────────────────────────

    #[test]
    fn test_next_section_after_exhausted_returns_none() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();

        // Repeated calls should all return None
        assert!(reader.next_section().unwrap().is_none());
        assert!(reader.next_section().unwrap().is_none());
        assert!(reader.next_section().unwrap().is_none());
    }

    #[test]
    fn test_skip_after_exhausted_returns_false() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();

        assert!(!reader.skip_section().unwrap());
        assert!(!reader.skip_section().unwrap());
    }

    #[test]
    fn test_peek_after_exhausted_returns_none() {
        let (buf, _hash) = write_minimal_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        reader.read_all_sections().unwrap();

        assert_eq!(reader.peek_section_type().unwrap(), None);
        assert_eq!(reader.peek_section_type().unwrap(), None);
    }

    #[test]
    fn test_interleaved_peek_skip_read() {
        let (buf, expected_hash) = write_full_change();
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();

        // Peek → read
        assert_eq!(
            reader.peek_section_type().unwrap(),
            Some(SectionType::Header)
        );
        let s = reader.next_section().unwrap().unwrap();
        assert_eq!(s.section_type, SectionType::Header);

        // Peek → skip
        assert_eq!(
            reader.peek_section_type().unwrap(),
            Some(SectionType::Dependencies)
        );
        reader.skip_section().unwrap();

        // Read without peek
        let s = reader.next_section().unwrap().unwrap();
        assert_eq!(s.section_type, SectionType::Provenance);

        // Skip without peek
        reader.skip_section().unwrap(); // GRAPH 1

        // Peek twice → read
        reader.peek_section_type().unwrap();
        reader.peek_section_type().unwrap();
        let s = reader.next_section().unwrap().unwrap();
        assert_eq!(s.section_type, SectionType::Graph); // GRAPH 2

        // Skip the rest
        while reader.skip_section().unwrap() {}

        // Verify
        let hash = reader.verify().unwrap();
        assert_eq!(hash, expected_hash);
    }
}

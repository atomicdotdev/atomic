//! Builder for [`FileHeader`] and the [`Trailer`] type.
//!
//! [`FileHeaderBuilder`] provides a fluent API for constructing file headers
//! with auto-computed flags. [`Trailer`] is the 32-byte blake3 hash at the
//! end of every V3 change file.

use super::super::error::{FormatResult, FORMAT_VERSION, MAGIC};
use super::header::{FileHeader, FileHeaderFlags};
use std::io::{Read, Write};

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

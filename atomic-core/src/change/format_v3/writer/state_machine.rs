//! The [`ChangeWriter`] state machine and its public API methods.
//!
//! This file contains the core writer struct, the internal state enum,
//! and all public methods for writing sections of a V3 change file.
//! Private helper methods live in the sibling `helpers` module.

use super::super::error::{FormatError, FormatResult};
use super::super::hash_table::HashDedupTable;
use super::super::types::{FileHeader, SectionHeader, SectionType, Trailer, HASH_INDEX_NONE};
use super::options::{WriteOutcome, WriterOptions, WriterStats};
use crate::change::header::ChangeHeader;
use crate::change::provenance::Provenance;
use std::io::Write;

// ═══════════════════════════════════════════════════════════════════════
// WriterState — internal state machine
// ═══════════════════════════════════════════════════════════════════════

/// Internal state tracking for the writer's state machine.
///
/// The writer progresses through these states in order. Each state
/// determines which write operations are valid next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriterState {
    /// Writer just created, nothing written yet.
    Created,

    /// File header has been written (64 bytes).
    FileHeaderWritten,

    /// Hash dedup table has been written. Hasher is now active.
    HashTableWritten,

    /// Currently writing metadata sections (HEADER, DEPS, PROVENANCE).
    /// Tracks which required sections have been written.
    WritingMetadata,

    /// Currently writing per-file GRAPH sections.
    WritingGraph,

    /// Currently writing per-file SEMANTIC sections.
    WritingSemantic,

    /// Currently writing CONTENT chunks.
    WritingContent,

    /// Wrote the UNHASHED section.
    WroteUnhashed,

    /// Trailer written, writer is consumed.
    Finalized,
}

impl WriterState {
    /// Returns a human-readable name for error messages.
    pub(super) fn name(self) -> &'static str {
        match self {
            WriterState::Created => "CREATED",
            WriterState::FileHeaderWritten => "FILE_HEADER_WRITTEN",
            WriterState::HashTableWritten => "HASH_TABLE_WRITTEN",
            WriterState::WritingMetadata => "WRITING_METADATA",
            WriterState::WritingGraph => "WRITING_GRAPH",
            WriterState::WritingSemantic => "WRITING_SEMANTIC",
            WriterState::WritingContent => "WRITING_CONTENT",
            WriterState::WroteUnhashed => "WROTE_UNHASHED",
            WriterState::Finalized => "FINALIZED",
        }
    }
}

/// Maps writer states to a numeric ordering for comparison.
pub(super) fn state_ordering(state: WriterState) -> u8 {
    match state {
        WriterState::Created => 0,
        WriterState::FileHeaderWritten => 1,
        WriterState::HashTableWritten => 2,
        WriterState::WritingMetadata => 3,
        WriterState::WritingGraph => 4,
        WriterState::WritingSemantic => 5,
        WriterState::WritingContent => 6,
        WriterState::WroteUnhashed => 7,
        WriterState::Finalized => 8,
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ChangeWriter — the main writer
// ═══════════════════════════════════════════════════════════════════════

/// Streaming writer for V3 change files.
///
/// Writes sections in order to the underlying writer, compressing each
/// section independently with zstd and computing the content hash
/// incrementally with blake3.
///
/// # State Machine
///
/// The writer enforces correct section ordering:
///
/// ```text
/// Created
///   │ write_file_header()
///   ▼
/// FileHeaderWritten
///   │ write_hash_table()
///   ▼
/// HashTableWritten
///   │ write_change_header() → write_dependencies() → [write_provenance()]
///   ▼
/// WritingMetadata
///   │ write_graph_section()* or advance to next state
///   ▼
/// WritingGraph
///   │ write_semantic_section()* or advance to next state
///   ▼
/// WritingSemantic
///   │ write_content_chunk()* or advance to next state
///   ▼
/// WritingContent
///   │ [write_unhashed()] or finalize()
///   ▼
/// Finalized
/// ```
///
/// Required sections: FileHeader, HashTable, HEADER (ChangeHeader), DEPS.
/// Optional sections: PROVENANCE, GRAPH*, SEMANTIC*, CONTENT*, UNHASHED.
///
/// # Thread Safety
///
/// `ChangeWriter` is NOT thread-safe. It wraps a `&mut W` writer and
/// maintains internal state (hasher, stats). For parallel compression,
/// compress sections in parallel first, then feed them to the writer
/// sequentially via [`ChangeWriter::write_compressed_section`].
pub struct ChangeWriter<'w, W: Write> {
    /// The underlying writer.
    pub(super) writer: &'w mut W,

    /// Current state in the state machine.
    pub(super) state: WriterState,

    /// Incremental blake3 hasher for computing the content hash.
    /// Initialized when the hash table is written.
    pub(super) hasher: blake3::Hasher,

    /// Writer configuration.
    pub(super) options: WriterOptions,

    /// Accumulated statistics.
    pub(super) stats: WriterStats,

    /// Tracks whether the HEADER section has been written.
    pub(super) wrote_header_section: bool,

    /// Tracks whether the DEPS section has been written.
    pub(super) wrote_deps_section: bool,

    /// The highest section ordering value written so far.
    /// Used to enforce monotonic section ordering.
    pub(super) last_section_ordering: Option<u8>,
}

impl<'w, W: Write> ChangeWriter<'w, W> {
    /// Create a new change writer wrapping the given output.
    ///
    /// The writer starts in the `Created` state. The first call must be
    /// [`write_file_header`](Self::write_file_header).
    ///
    /// # Arguments
    ///
    /// * `writer` - The destination for the serialized change file.
    /// * `options` - Configuration (compression level, etc.).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{ChangeWriter, WriterOptions};
    ///
    /// let mut buf = Vec::new();
    /// let writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    /// ```
    pub fn new(writer: &'w mut W, options: WriterOptions) -> Self {
        Self {
            writer,
            state: WriterState::Created,
            hasher: blake3::Hasher::new(),
            options,
            stats: WriterStats::default(),
            wrote_header_section: false,
            wrote_deps_section: false,
            last_section_ordering: None,
        }
    }

    /// Returns the current writer statistics.
    ///
    /// Statistics are updated after each section write.
    pub fn stats(&self) -> &WriterStats {
        &self.stats
    }

    /// Returns the current state of the writer (for debugging).
    pub fn state_name(&self) -> &'static str {
        self.state.name()
    }

    // ── Step 1: File Header ────────────────────────────────────────

    /// Write the 64-byte file header.
    ///
    /// This MUST be the first write operation. The header is written
    /// uncompressed and is NOT included in the content hash.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if not in `Created` state.
    /// - I/O errors from the underlying writer.
    pub fn write_file_header(&mut self, header: &FileHeader) -> FormatResult<()> {
        self.expect_state(WriterState::Created, "write_file_header")?;

        header.write_to(self.writer)?;
        self.stats.total_bytes_written += FileHeader::SIZE as u64;
        self.state = WriterState::FileHeaderWritten;
        Ok(())
    }

    // ── Step 2: Hash Dedup Table ───────────────────────────────────

    /// Write the hash deduplication table.
    ///
    /// The hash table is written uncompressed immediately after the file
    /// header. Its bytes ARE fed through the blake3 hasher (they contribute
    /// to the content hash).
    ///
    /// This must be called after [`write_file_header`](Self::write_file_header)
    /// and before any section writes.
    ///
    /// # Arguments
    ///
    /// * `table` - The hash dedup table to write.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if not in `FileHeaderWritten` state.
    /// - I/O errors from the underlying writer.
    pub fn write_hash_table(&mut self, table: &HashDedupTable) -> FormatResult<()> {
        self.expect_state(WriterState::FileHeaderWritten, "write_hash_table")?;

        // Write hash table bytes
        let mut hash_table_bytes = Vec::with_capacity(table.serialized_size());
        table.write_to(&mut hash_table_bytes)?;

        // Feed through hasher (hash table IS part of the content hash)
        self.hasher.update(&hash_table_bytes);

        // Write to output
        self.writer.write_all(&hash_table_bytes)?;
        self.stats.total_bytes_written += hash_table_bytes.len() as u64;

        self.state = WriterState::HashTableWritten;
        Ok(())
    }

    // ── Step 3: Metadata Sections ──────────────────────────────────

    /// Write the HEADER section containing the [`ChangeHeader`].
    ///
    /// This is a required section and must be the first section after the
    /// hash table. The `ChangeHeader` is serialized with postcard, compressed
    /// with zstd, and its compressed bytes are fed through the blake3 hasher.
    ///
    /// # Arguments
    ///
    /// * `header` - The change metadata (message, authors, timestamp).
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if not in `HashTableWritten` state.
    /// - Postcard serialization errors.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_change_header(&mut self, header: &ChangeHeader) -> FormatResult<()> {
        if self.state != WriterState::HashTableWritten && self.state != WriterState::WritingMetadata
        {
            return Err(FormatError::UnexpectedSection {
                got: "HEADER".to_string(),
                expected: format!(
                    "state HASH_TABLE_WRITTEN or WRITING_METADATA, but was {}",
                    self.state.name()
                ),
            });
        }

        if self.wrote_header_section {
            return Err(FormatError::UnexpectedSection {
                got: "HEADER (duplicate)".to_string(),
                expected: "HEADER section already written".to_string(),
            });
        }

        self.write_postcard_section(SectionType::Header, header)?;
        self.wrote_header_section = true;
        self.state = WriterState::WritingMetadata;
        Ok(())
    }

    /// Write the DEPS section containing dependency hash indices.
    ///
    /// This is a required section. It contains the list of dependency hashes
    /// as `HashIndex` values (referencing the hash dedup table). Even if
    /// there are no dependencies, an empty deps section must be written.
    ///
    /// # Arguments
    ///
    /// * `dependency_indices` - Indices into the hash dedup table for each
    ///   dependency. Use [`HashDedupTable::lookup`] or [`HashDedupTable::require`]
    ///   to convert `Hash` values to indices.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if HEADER hasn't been written yet.
    /// - Postcard serialization errors.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_dependencies(&mut self, dependency_indices: &[u16]) -> FormatResult<()> {
        if self.state != WriterState::WritingMetadata {
            return Err(FormatError::UnexpectedSection {
                got: "DEPS".to_string(),
                expected: format!("state WRITING_METADATA, but was {}", self.state.name()),
            });
        }
        if !self.wrote_header_section {
            return Err(FormatError::UnexpectedSection {
                got: "DEPS".to_string(),
                expected: "HEADER section must be written first".to_string(),
            });
        }
        if self.wrote_deps_section {
            return Err(FormatError::UnexpectedSection {
                got: "DEPS (duplicate)".to_string(),
                expected: "DEPS section already written".to_string(),
            });
        }

        // Filter out HASH_INDEX_NONE — it's a sentinel, not a real dependency
        let clean: Vec<u16> = dependency_indices
            .iter()
            .copied()
            .filter(|&i| i != HASH_INDEX_NONE)
            .collect();

        self.write_postcard_section(SectionType::Dependencies, &clean)?;
        self.wrote_deps_section = true;
        Ok(())
    }

    /// Write the optional PROVENANCE section.
    ///
    /// Contains AI provenance metadata for the change. This section is
    /// optional — omit it if there's no AI provenance to record.
    ///
    /// # Arguments
    ///
    /// * `provenance` - List of provenance entries.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if DEPS hasn't been written yet,
    ///   or if PROVENANCE was already written.
    /// - Postcard serialization errors.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_provenance(&mut self, provenance: &[Provenance]) -> FormatResult<()> {
        if self.state != WriterState::WritingMetadata {
            return Err(FormatError::UnexpectedSection {
                got: "PROVENANCE".to_string(),
                expected: format!("state WRITING_METADATA, but was {}", self.state.name()),
            });
        }
        if !self.wrote_deps_section {
            return Err(FormatError::UnexpectedSection {
                got: "PROVENANCE".to_string(),
                expected: "DEPS section must be written first".to_string(),
            });
        }

        // Wrap in a Vec for serialization (slices are unsized)
        let provenance_vec: Vec<&Provenance> = provenance.iter().collect();
        self.write_postcard_section(SectionType::Provenance, &provenance_vec)?;
        Ok(())
    }

    // ── Steps 4-7 (graph, semantic, content, unhashed) are in sections.rs ──

    // ── Step 8: Finalize ───────────────────────────────────────────

    /// Finalize the change file by writing the trailer.
    ///
    /// This computes the final blake3 content hash from all hashed sections
    /// written so far, writes the 32-byte trailer, and returns the
    /// [`WriteOutcome`] containing the hash and statistics.
    ///
    /// After calling this, the writer is consumed and no more writes are possible.
    ///
    /// # Required Sections
    ///
    /// The following sections must have been written before finalizing:
    /// - File header (via [`write_file_header`](Self::write_file_header))
    /// - Hash table (via [`write_hash_table`](Self::write_hash_table))
    /// - HEADER section (via [`write_change_header`](Self::write_change_header))
    /// - DEPS section (via [`write_dependencies`](Self::write_dependencies))
    ///
    /// # Returns
    ///
    /// A [`WriteOutcome`] containing the 32-byte content hash and writing statistics.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if required sections are missing.
    /// - I/O errors from writing the trailer.
    pub fn finalize(mut self) -> FormatResult<WriteOutcome> {
        self.ensure_metadata_complete()?;

        // Compute the final hash
        let hash = self.hasher.finalize();
        let content_hash = *hash.as_bytes();

        // Write the trailer
        let trailer = Trailer { content_hash };
        trailer.write_to(self.writer)?;
        self.stats.total_bytes_written += Trailer::SIZE as u64;

        self.state = WriterState::Finalized;

        Ok(WriteOutcome {
            content_hash,
            stats: self.stats,
        })
    }

    // ── Low-Level: Pre-Compressed Section ──────────────────────────

    /// Write a pre-compressed section.
    ///
    /// This is the lowest-level write API. It writes a section header +
    /// compressed payload, feeding the compressed bytes through the hasher
    /// (unless the section is UNHASHED).
    ///
    /// Use this when you've already compressed the section data externally
    /// (e.g., via parallel compression with rayon).
    ///
    /// # Arguments
    ///
    /// * `section_type` - The type of section.
    /// * `compressed_data` - Already-compressed section payload.
    /// * `uncompressed_len` - Length of the original uncompressed data (for stats).
    ///
    /// # Errors
    ///
    /// - Section ordering violations.
    /// - I/O errors.
    pub fn write_compressed_section(
        &mut self,
        section_type: SectionType,
        compressed_data: &[u8],
        uncompressed_len: u64,
    ) -> FormatResult<()> {
        self.ensure_metadata_complete()?;
        self.check_section_ordering(section_type)?;

        let section_header = SectionHeader::new(section_type, compressed_data.len() as u32);
        section_header.write_to(self.writer)?;
        self.writer.write_all(compressed_data)?;

        // Feed through hasher if this is a hashed section
        if section_type.is_hashed() {
            let header_bytes = section_header.to_bytes();
            self.hasher.update(&header_bytes);
            self.hasher.update(compressed_data);
        }

        self.stats.sections_written += 1;
        self.stats.total_uncompressed += uncompressed_len;
        self.stats.total_compressed += compressed_data.len() as u64;
        self.stats.total_bytes_written += (SectionHeader::SIZE + compressed_data.len()) as u64;

        // Update per-type counters
        match section_type {
            SectionType::Graph => self.stats.graph_sections_written += 1,
            SectionType::Semantic => self.stats.semantic_sections_written += 1,
            SectionType::Content => self.stats.content_chunks_written += 1,
            _ => {}
        }

        Ok(())
    }
}

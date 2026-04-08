//! Per-file section writing methods for [`ChangeWriter`].
//!
//! These methods handle writing GRAPH, SEMANTIC, CONTENT, and UNHASHED
//! sections. They are split from the core state machine to keep each
//! file under 500 lines.

use super::super::error::{FormatError, FormatResult};
use super::super::types::{ContentChunkHeader, SectionHeader, SectionType};
use super::state_machine::{ChangeWriter, WriterState};
use std::io::Write;

impl<'w, W: Write> ChangeWriter<'w, W> {
    // ── Step 4: Per-File Graph Sections ────────────────────────────

    /// Write a GRAPH section for a single file.
    ///
    /// Each modified file in the change gets one GRAPH section containing
    /// its graph operations (vertex insertions, edge updates). The payload
    /// is provided as pre-serialized bytes (postcard-encoded) — the writer
    /// compresses and hashes them.
    ///
    /// # Arguments
    ///
    /// * `payload` - Postcard-serialized graph section data (uncompressed).
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if called before DEPS,
    ///   or after SEMANTIC/CONTENT/UNHASHED sections.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_graph_section(&mut self, payload: &[u8]) -> FormatResult<()> {
        self.ensure_metadata_complete()?;
        self.transition_to_at_least(WriterState::WritingGraph)?;

        if self.state != WriterState::WritingGraph {
            return Err(FormatError::UnexpectedSection {
                got: "GRAPH".to_string(),
                expected: format!("state WRITING_GRAPH, but was {}", self.state.name()),
            });
        }

        self.write_raw_section(SectionType::Graph, payload)?;
        self.stats.graph_sections_written += 1;
        Ok(())
    }

    // ── Step 5: Per-File Semantic Sections ─────────────────────────

    /// Write a SEMANTIC section for a single file.
    ///
    /// Each modified file can have one SEMANTIC section containing its
    /// Trunk/Branch/Leaf operations for code review, diff display, and blame.
    /// Semantic sections are optional — a "thin" change omits them.
    ///
    /// # Arguments
    ///
    /// * `payload` - Postcard-serialized semantic section data (uncompressed).
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if called before DEPS,
    ///   or after CONTENT/UNHASHED sections.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_semantic_section(&mut self, payload: &[u8]) -> FormatResult<()> {
        self.ensure_metadata_complete()?;
        self.transition_to_at_least(WriterState::WritingSemantic)?;

        if self.state != WriterState::WritingSemantic {
            return Err(FormatError::UnexpectedSection {
                got: "SEMANTIC".to_string(),
                expected: format!("state WRITING_SEMANTIC, but was {}", self.state.name()),
            });
        }

        self.write_raw_section(SectionType::Semantic, payload)?;
        self.stats.semantic_sections_written += 1;
        Ok(())
    }

    // ── Step 6: Content Chunks ─────────────────────────────────────

    /// Write a CONTENT chunk.
    ///
    /// Content is split into chunks (typically via FastCDC). Each chunk is
    /// independently compressed and content-addressed by its blake3 hash.
    /// Content chunks use an extended header ([`ContentChunkHeader`]) that
    /// includes the chunk index and hash.
    ///
    /// # Arguments
    ///
    /// * `chunk_index` - Sequential chunk number (0-based).
    /// * `data` - The uncompressed chunk data.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if called before DEPS,
    ///   or after UNHASHED.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_content_chunk(&mut self, chunk_index: u32, data: &[u8]) -> FormatResult<()> {
        self.ensure_metadata_complete()?;
        self.transition_to_at_least(WriterState::WritingContent)?;

        if self.state != WriterState::WritingContent {
            return Err(FormatError::UnexpectedSection {
                got: "CONTENT".to_string(),
                expected: format!("state WRITING_CONTENT, but was {}", self.state.name()),
            });
        }

        // Compute chunk hash (over uncompressed data)
        let chunk_hash = *blake3::hash(data).as_bytes();

        // Compress the chunk data
        let compressed = zstd::encode_all(data, self.options.compression_level())
            .map_err(|e| FormatError::Compress(e.to_string()))?;

        // Build the content chunk header
        let chunk_header = ContentChunkHeader::new(
            chunk_index,
            chunk_hash,
            data.len() as u32,
            compressed.len() as u32,
        );

        // Write the chunk header
        let header_bytes = chunk_header.to_bytes();
        self.writer.write_all(&header_bytes)?;

        // Feed chunk header + compressed data through the content hasher
        self.hasher.update(&header_bytes);
        self.hasher.update(&compressed);

        // Write compressed data
        self.writer.write_all(&compressed)?;

        // Update stats
        self.stats.sections_written += 1;
        self.stats.content_chunks_written += 1;
        self.stats.total_uncompressed += data.len() as u64;
        self.stats.total_compressed += compressed.len() as u64;
        self.stats.total_bytes_written += (ContentChunkHeader::SIZE + compressed.len()) as u64;

        Ok(())
    }

    // ── Step 7: Unhashed Section ───────────────────────────────────

    /// Write the optional UNHASHED section.
    ///
    /// This section is NOT included in the content hash. It's intended for
    /// data that shouldn't affect the change's identity: AI transcripts,
    /// reasoning traces, editor state, review comments, etc.
    ///
    /// The payload is provided as raw bytes (typically JSON-serialized).
    /// The writer compresses it with zstd but does NOT feed it through
    /// the blake3 hasher.
    ///
    /// # Arguments
    ///
    /// * `data` - The unhashed payload (uncompressed). Typically
    ///   `serde_json::to_vec(&value)?`.
    ///
    /// # Errors
    ///
    /// - [`FormatError::UnexpectedSection`] if called after finalize or
    ///   if UNHASHED was already written.
    /// - Zstd compression errors.
    /// - I/O errors.
    pub fn write_unhashed(&mut self, data: &[u8]) -> FormatResult<()> {
        self.ensure_metadata_complete()?;
        self.transition_to_at_least(WriterState::WritingContent)?;

        if self.state == WriterState::WroteUnhashed || self.state == WriterState::Finalized {
            return Err(FormatError::UnexpectedSection {
                got: "UNHASHED".to_string(),
                expected: format!(
                    "UNHASHED can only be written once, state is {}",
                    self.state.name()
                ),
            });
        }

        // Compress the data
        let compressed = zstd::encode_all(data, self.options.compression_level())
            .map_err(|e| FormatError::Compress(e.to_string()))?;

        // Write section header + compressed payload
        let section_header = SectionHeader::new(SectionType::Unhashed, compressed.len() as u32);
        section_header.write_to(self.writer)?;
        self.writer.write_all(&compressed)?;

        // NOTE: Do NOT feed through hasher — unhashed section is excluded
        // from the content hash by design.

        self.stats.sections_written += 1;
        self.stats.total_uncompressed += data.len() as u64;
        self.stats.total_compressed += compressed.len() as u64;
        self.stats.total_bytes_written += (SectionHeader::SIZE + compressed.len()) as u64;

        self.state = WriterState::WroteUnhashed;
        Ok(())
    }
}

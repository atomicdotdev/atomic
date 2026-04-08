//! Private helper methods for [`ChangeWriter`].
//!
//! These methods handle state validation, section ordering enforcement,
//! and the shared compress-hash-write pipeline used by all section writers.

use super::super::error::{FormatError, FormatResult};
use super::super::types::{SectionHeader, SectionType};
use super::state_machine::{state_ordering, ChangeWriter, WriterState};
use serde::Serialize;
use std::io::Write;

impl<'w, W: Write> ChangeWriter<'w, W> {
    /// Check that the writer is in the expected state.
    pub(super) fn expect_state(&self, expected: WriterState, operation: &str) -> FormatResult<()> {
        if self.state != expected {
            return Err(FormatError::UnexpectedSection {
                got: operation.to_string(),
                expected: format!("state {}, but was {}", expected.name(), self.state.name()),
            });
        }
        Ok(())
    }

    /// Ensure that the required metadata sections (HEADER + DEPS) have been written.
    pub(super) fn ensure_metadata_complete(&self) -> FormatResult<()> {
        if !self.wrote_header_section {
            return Err(FormatError::UnexpectedSection {
                got: "finalize or write section".to_string(),
                expected: "HEADER section must be written first".to_string(),
            });
        }
        if !self.wrote_deps_section {
            return Err(FormatError::UnexpectedSection {
                got: "finalize or write section".to_string(),
                expected: "DEPS section must be written first".to_string(),
            });
        }
        Ok(())
    }

    /// Advance the state machine to at least the given state.
    ///
    /// This handles the forward transitions: if we're in WritingMetadata
    /// and asked to write a SEMANTIC section, we advance through WritingGraph
    /// to WritingSemantic (since it's valid to have zero GRAPH sections).
    pub(super) fn transition_to_at_least(&mut self, target: WriterState) -> FormatResult<()> {
        let target_ord = state_ordering(target);
        let current_ord = state_ordering(self.state);

        if target_ord < current_ord {
            // Going backward — not allowed
            return Err(FormatError::UnexpectedSection {
                got: target.name().to_string(),
                expected: format!(
                    "state {} or later (cannot go back to {})",
                    self.state.name(),
                    target.name()
                ),
            });
        }

        // Advance state forward
        if target_ord > current_ord {
            self.state = target;
        }
        Ok(())
    }

    /// Check that a section type's ordering is valid relative to the last section.
    pub(super) fn check_section_ordering(&mut self, section_type: SectionType) -> FormatResult<()> {
        let ordering = section_type.ordering();

        if let Some(last) = self.last_section_ordering {
            if ordering < last {
                return Err(FormatError::UnexpectedSection {
                    got: section_type.name().to_string(),
                    expected: format!(
                        "section with ordering >= {} (last written had ordering {})",
                        last, last
                    ),
                });
            }
        }

        self.last_section_ordering = Some(ordering);
        Ok(())
    }

    /// Serialize a value with postcard, compress with zstd, hash, and write.
    ///
    /// This is the standard flow for typed metadata sections (HEADER, DEPS,
    /// PROVENANCE) and will be extended for GRAPH/SEMANTIC in Phase 2.
    pub(super) fn write_postcard_section<T: Serialize>(
        &mut self,
        section_type: SectionType,
        value: &T,
    ) -> FormatResult<()> {
        // Serialize with postcard
        let serialized = postcard::to_allocvec(value)?;

        // Compress with zstd
        let compressed = zstd::encode_all(&serialized[..], self.options.compression_level())
            .map_err(|e| FormatError::Compress(e.to_string()))?;

        // Write section header
        let section_header = SectionHeader::new(section_type, compressed.len() as u32);
        let header_bytes = section_header.to_bytes();
        self.writer.write_all(&header_bytes)?;

        // Write compressed payload
        self.writer.write_all(&compressed)?;

        // Feed through hasher (section header + compressed data)
        if section_type.is_hashed() {
            self.hasher.update(&header_bytes);
            self.hasher.update(&compressed);
        }

        // Update stats
        self.stats.sections_written += 1;
        self.stats.total_uncompressed += serialized.len() as u64;
        self.stats.total_compressed += compressed.len() as u64;
        self.stats.total_bytes_written += (SectionHeader::SIZE + compressed.len()) as u64;

        self.last_section_ordering = Some(section_type.ordering());
        Ok(())
    }

    /// Compress raw bytes with zstd, hash, and write as a section.
    ///
    /// Used for GRAPH and SEMANTIC sections where the caller provides
    /// pre-serialized (but uncompressed) postcard bytes.
    pub(super) fn write_raw_section(
        &mut self,
        section_type: SectionType,
        uncompressed: &[u8],
    ) -> FormatResult<()> {
        // Compress with zstd
        let compressed = zstd::encode_all(uncompressed, self.options.compression_level())
            .map_err(|e| FormatError::Compress(e.to_string()))?;

        // Write section header
        let section_header = SectionHeader::new(section_type, compressed.len() as u32);
        let header_bytes = section_header.to_bytes();
        self.writer.write_all(&header_bytes)?;

        // Write compressed payload
        self.writer.write_all(&compressed)?;

        // Feed through hasher
        if section_type.is_hashed() {
            self.hasher.update(&header_bytes);
            self.hasher.update(&compressed);
        }

        // Update stats
        self.stats.sections_written += 1;
        self.stats.total_uncompressed += uncompressed.len() as u64;
        self.stats.total_compressed += compressed.len() as u64;
        self.stats.total_bytes_written += (SectionHeader::SIZE + compressed.len()) as u64;

        self.last_section_ordering = Some(section_type.ordering());
        Ok(())
    }
}

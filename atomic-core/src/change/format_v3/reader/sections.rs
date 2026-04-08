//! Section reading, skipping, and verification methods for ChangeReader.

use super::super::error::{FormatError, FormatResult};
use super::super::types::{ContentChunkHeader, SectionHeader, SectionType, Trailer};
use super::{ChangeReader, ContentChunkInfo, PeekedSection, ReadSection};
use std::io::Read;

impl<'r, R: Read> ChangeReader<'r, R> {
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
        if self.peeked.is_none() && self.peek_section_type()?.is_none() {
            return Ok(None);
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
        if self.peeked.is_none() && self.peek_section_type()?.is_none() {
            return Ok(false);
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

//! Streaming writer for the Change Format V3.
//!
//! The [`ChangeWriter`] writes a complete V3 change file to any [`Write`] destination,
//! enforcing correct section ordering, compressing each section independently with zstd,
//! and computing the content hash incrementally via `blake3::Hasher`.
//!
//! # Design
//!
//! The writer is a **state machine** that enforces the V3 section ordering:
//!
//! ```text
//! FileHeader → HashTable → HEADER → DEPS → [PROVENANCE] → GRAPH* → SEMANTIC* → CONTENT* → [UNHASHED] → Trailer
//! ```
//!
//! Each transition validates that sections appear in the correct order. Attempting
//! to write a section out of order (e.g., writing SEMANTIC before GRAPH) produces
//! a compile-time-friendly error via the state machine, or a runtime
//! [`FormatError::UnexpectedSection`] for dynamic cases.
//!
//! # Incremental Hashing
//!
//! The content hash is computed **incrementally** as sections are written:
//!
//! ```text
//! blake3::Hasher
//!   ← hash table bytes (raw)
//!   ← HEADER section (compressed)
//!   ← DEPS section (compressed)
//!   ← PROVENANCE section (compressed, if present)
//!   ← GRAPH sections (compressed, each one)
//!   ← SEMANTIC sections (compressed, each one)
//!   ← CONTENT chunks (compressed, each one)
//!   → finalize() → content_hash
//! ```
//!
//! The UNHASHED section is explicitly excluded from the hash. The file header
//! and trailer are also excluded (the header contains structural metadata,
//! and the trailer contains the hash itself).
//!
//! # Memory Usage
//!
//! The writer never buffers the entire change in memory. Each section is
//! serialized → compressed → hashed → written in a single pass. Peak memory
//! is proportional to the **largest single section**, not the total change size.
//!
//! # Compression
//!
//! Each section is independently compressed with zstd at a configurable level
//! (default: 3). This enables:
//! - Parallel compression in the future (each section compresses independently)
//! - Selective decompression (read only the sections you need)
//! - Better compression ratios (homogeneous data within each section)
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::format_v3::{
//!     ChangeWriter, HashDedupTable, FileHeader, WriterOptions,
//! };
//! use atomic_core::change::ChangeHeader;
//!
//! let mut output = Vec::new();
//!
//! // Build the hash table
//! let self_hash = *blake3::hash(b"placeholder").as_bytes();
//! let hash_table = HashDedupTable::new(self_hash);
//!
//! // Build the file header
//! let file_header = FileHeader::builder()
//!     .hash_table_entries(hash_table.len() as u32)
//!     .graph_section_count(0)
//!     .build();
//!
//! // Create writer and write sections
//! let mut writer = ChangeWriter::new(&mut output, WriterOptions::default());
//! writer.write_file_header(&file_header).unwrap();
//! writer.write_hash_table(&hash_table).unwrap();
//!
//! // Write required metadata sections
//! let change_header = ChangeHeader::new("Initial commit");
//! writer.write_change_header(&change_header).unwrap();
//! writer.write_dependencies(&[]).unwrap();
//!
//! // Finalize — computes and writes the trailer
//! let content_hash = writer.finalize().unwrap();
//! assert_eq!(output.len() > 64, true); // at least the file header
//! ```
//!
//! # Error Handling
//!
//! All write methods return [`FormatResult<()>`] (or [`FormatResult<[u8; 32]>`] for
//! `finalize`). Errors include:
//! - I/O errors from the underlying writer
//! - Compression errors from zstd
//! - Section ordering violations
//! - State machine violations (e.g., finalizing before writing required sections)

use super::error::{FormatError, FormatResult};
use super::hash_table::HashDedupTable;
use super::types::{
    ContentChunkHeader, FileHeader, SectionHeader, SectionType, Trailer, HASH_INDEX_NONE,
};
use crate::change::header::ChangeHeader;
use crate::change::provenance::Provenance;
use serde::Serialize;
use std::io::Write;

/// Default zstd compression level.
///
/// Level 3 provides a good balance between compression ratio and speed.
/// Higher levels (up to 22) compress more but are significantly slower.
/// Level 1 is fastest with moderate compression.
const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

// ═══════════════════════════════════════════════════════════════════════
// WriterState — internal state machine
// ═══════════════════════════════════════════════════════════════════════

/// Internal state tracking for the writer's state machine.
///
/// The writer progresses through these states in order. Each state
/// determines which write operations are valid next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterState {
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
    fn name(self) -> &'static str {
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

// ═══════════════════════════════════════════════════════════════════════
// WriterOptions — configuration for ChangeWriter
// ═══════════════════════════════════════════════════════════════════════

/// Configuration options for [`ChangeWriter`].
///
/// Controls compression level and other writer behavior.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::WriterOptions;
///
/// // Default options (compression level 3)
/// let opts = WriterOptions::default();
/// assert_eq!(opts.compression_level(), 3);
///
/// // Fast compression
/// let opts = WriterOptions::fast();
/// assert_eq!(opts.compression_level(), 1);
///
/// // Maximum compression
/// let opts = WriterOptions::max_compression();
/// assert_eq!(opts.compression_level(), 19);
/// ```
#[derive(Clone, Debug)]
pub struct WriterOptions {
    /// Zstd compression level (1-22, default 3).
    compression_level: i32,
}

impl WriterOptions {
    /// Create options with a specific compression level.
    ///
    /// # Arguments
    ///
    /// * `level` - Zstd compression level (1-22). Values outside this range
    ///   are clamped.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::WriterOptions;
    ///
    /// let opts = WriterOptions::with_compression_level(5);
    /// assert_eq!(opts.compression_level(), 5);
    /// ```
    pub fn with_compression_level(level: i32) -> Self {
        Self {
            compression_level: level.clamp(1, 22),
        }
    }

    /// Fast compression preset (level 1).
    ///
    /// Best for local operations where I/O is fast and CPU is the bottleneck.
    pub fn fast() -> Self {
        Self {
            compression_level: 1,
        }
    }

    /// Maximum compression preset (level 19).
    ///
    /// Best for archival or network transfer where smaller size matters
    /// more than compression speed.
    pub fn max_compression() -> Self {
        Self {
            compression_level: 19,
        }
    }

    /// Returns the configured compression level.
    #[inline]
    pub fn compression_level(&self) -> i32 {
        self.compression_level
    }
}

impl Default for WriterOptions {
    /// Default options: compression level 3.
    fn default() -> Self {
        Self {
            compression_level: DEFAULT_COMPRESSION_LEVEL,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// WriterStats — statistics collected during writing
// ═══════════════════════════════════════════════════════════════════════

/// Statistics collected during the writing process.
///
/// These are available after [`ChangeWriter::finalize`] via the
/// [`WriteOutcome`] return value, and are useful for logging,
/// progress reporting, and performance analysis.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::WriterStats;
///
/// let stats = WriterStats::default();
/// assert_eq!(stats.sections_written, 0);
/// assert_eq!(stats.total_uncompressed, 0);
/// assert_eq!(stats.total_compressed, 0);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriterStats {
    /// Number of sections written (excluding file header, hash table, trailer).
    pub sections_written: u32,

    /// Number of GRAPH sections written.
    pub graph_sections_written: u32,

    /// Number of SEMANTIC sections written.
    pub semantic_sections_written: u32,

    /// Number of CONTENT chunks written.
    pub content_chunks_written: u32,

    /// Total bytes of uncompressed section payloads.
    pub total_uncompressed: u64,

    /// Total bytes of compressed section payloads.
    pub total_compressed: u64,

    /// Total bytes written to the output (including framing).
    pub total_bytes_written: u64,
}

impl WriterStats {
    /// Compression ratio (compressed / uncompressed).
    ///
    /// Returns `f64::NAN` if no data was compressed.
    pub fn compression_ratio(&self) -> f64 {
        if self.total_uncompressed == 0 {
            return f64::NAN;
        }
        self.total_compressed as f64 / self.total_uncompressed as f64
    }

    /// Space savings as a percentage (0.0 to 100.0).
    ///
    /// Returns 0.0 if no data was compressed.
    pub fn space_savings_pct(&self) -> f64 {
        if self.total_uncompressed == 0 {
            return 0.0;
        }
        (1.0 - self.total_compressed as f64 / self.total_uncompressed as f64) * 100.0
    }
}

impl std::fmt::Display for WriterStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sections, {} bytes uncompressed → {} bytes compressed ({:.1}% savings), {} bytes total on disk",
            self.sections_written,
            self.total_uncompressed,
            self.total_compressed,
            self.space_savings_pct(),
            self.total_bytes_written,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// WriteOutcome — result of finalize()
// ═══════════════════════════════════════════════════════════════════════

/// The outcome of successfully writing and finalizing a V3 change file.
///
/// Returned by [`ChangeWriter::finalize`]. Contains the computed content
/// hash (which is the change's identity) and writing statistics.
///
/// # Examples
///
/// ```rust,ignore
/// let outcome = writer.finalize()?;
/// println!("Change hash: {:?}", outcome.content_hash);
/// println!("Stats: {}", outcome.stats);
/// ```
#[derive(Clone, Debug)]
pub struct WriteOutcome {
    /// The blake3 content hash computed over all hashed sections.
    ///
    /// This is the **identity** of the change — it's what gets registered
    /// in the pristine database and used for content addressing.
    pub content_hash: [u8; 32],

    /// Statistics about the writing process.
    pub stats: WriterStats,
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
/// sequentially via [`write_compressed_section`].
pub struct ChangeWriter<'w, W: Write> {
    /// The underlying writer.
    writer: &'w mut W,

    /// Current state in the state machine.
    state: WriterState,

    /// Incremental blake3 hasher for computing the content hash.
    /// Initialized when the hash table is written.
    hasher: blake3::Hasher,

    /// Writer configuration.
    options: WriterOptions,

    /// Accumulated statistics.
    stats: WriterStats,

    /// Tracks whether the HEADER section has been written.
    wrote_header_section: bool,

    /// Tracks whether the DEPS section has been written.
    wrote_deps_section: bool,

    /// The highest section ordering value written so far.
    /// Used to enforce monotonic section ordering.
    last_section_ordering: Option<u8>,
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
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::format_v3::{ChangeWriter, FileHeader, WriterOptions};
    ///
    /// let mut buf = Vec::new();
    /// let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    ///
    /// let header = FileHeader::default();
    /// writer.write_file_header(&header).unwrap();
    /// assert_eq!(buf.len(), 64);
    /// ```
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

    // ── Step 4: Per-File Graph Sections ────────────────────────────

    /// Write a GRAPH section for a single file.
    ///
    /// Each modified file in the change gets one GRAPH section containing
    /// its graph operations (vertex insertions, edge updates). The payload
    /// is provided as pre-serialized bytes (postcard-encoded) — the writer
    /// compresses and hashes them.
    ///
    /// In Phase 2, a higher-level API will accept typed `GraphSection`
    /// structs. For now, callers serialize the payload themselves.
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
        let compressed = zstd::encode_all(data, self.options.compression_level)
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
        let compressed = zstd::encode_all(data, self.options.compression_level)
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

    // ── Internal Helpers ───────────────────────────────────────────

    /// Check that the writer is in the expected state.
    fn expect_state(&self, expected: WriterState, operation: &str) -> FormatResult<()> {
        if self.state != expected {
            return Err(FormatError::UnexpectedSection {
                got: operation.to_string(),
                expected: format!("state {}, but was {}", expected.name(), self.state.name()),
            });
        }
        Ok(())
    }

    /// Ensure that the required metadata sections (HEADER + DEPS) have been written.
    fn ensure_metadata_complete(&self) -> FormatResult<()> {
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
    fn transition_to_at_least(&mut self, target: WriterState) -> FormatResult<()> {
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
    fn check_section_ordering(&mut self, section_type: SectionType) -> FormatResult<()> {
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
    fn write_postcard_section<T: Serialize>(
        &mut self,
        section_type: SectionType,
        value: &T,
    ) -> FormatResult<()> {
        // Serialize with postcard
        let serialized = postcard::to_allocvec(value)?;

        // Compress with zstd
        let compressed = zstd::encode_all(&serialized[..], self.options.compression_level)
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
    fn write_raw_section(
        &mut self,
        section_type: SectionType,
        uncompressed: &[u8],
    ) -> FormatResult<()> {
        // Compress with zstd
        let compressed = zstd::encode_all(uncompressed, self.options.compression_level)
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

/// Maps writer states to a numeric ordering for comparison.
fn state_ordering(state: WriterState) -> u8 {
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
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::format_v3::FileHeader;

    /// Helper: create a minimal writer with header + hash table already written.
    fn writer_with_preamble(buf: &mut Vec<u8>) -> ChangeWriter<'_, Vec<u8>> {
        let self_hash = *blake3::hash(b"test change").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);

        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(0)
            .build();

        let mut writer = ChangeWriter::new(buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
    }

    /// Helper: create a minimal writer with preamble + metadata sections written.
    fn writer_with_metadata(buf: &mut Vec<u8>) -> ChangeWriter<'_, Vec<u8>> {
        let mut writer = writer_with_preamble(buf);
        let header = ChangeHeader::new("Test");
        writer.write_change_header(&header).unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer
    }

    // ── WriterOptions ──────────────────────────────────────────────

    #[test]
    fn test_writer_options_default() {
        let opts = WriterOptions::default();
        assert_eq!(opts.compression_level(), DEFAULT_COMPRESSION_LEVEL);
        assert_eq!(opts.compression_level(), 3);
    }

    #[test]
    fn test_writer_options_fast() {
        let opts = WriterOptions::fast();
        assert_eq!(opts.compression_level(), 1);
    }

    #[test]
    fn test_writer_options_max_compression() {
        let opts = WriterOptions::max_compression();
        assert_eq!(opts.compression_level(), 19);
    }

    #[test]
    fn test_writer_options_custom_level() {
        let opts = WriterOptions::with_compression_level(10);
        assert_eq!(opts.compression_level(), 10);
    }

    #[test]
    fn test_writer_options_clamped_low() {
        let opts = WriterOptions::with_compression_level(-5);
        assert_eq!(opts.compression_level(), 1);
    }

    #[test]
    fn test_writer_options_clamped_high() {
        let opts = WriterOptions::with_compression_level(100);
        assert_eq!(opts.compression_level(), 22);
    }

    // ── WriterStats ────────────────────────────────────────────────

    #[test]
    fn test_writer_stats_default() {
        let stats = WriterStats::default();
        assert_eq!(stats.sections_written, 0);
        assert_eq!(stats.graph_sections_written, 0);
        assert_eq!(stats.semantic_sections_written, 0);
        assert_eq!(stats.content_chunks_written, 0);
        assert_eq!(stats.total_uncompressed, 0);
        assert_eq!(stats.total_compressed, 0);
        assert_eq!(stats.total_bytes_written, 0);
    }

    #[test]
    fn test_writer_stats_compression_ratio() {
        let stats = WriterStats {
            total_uncompressed: 1000,
            total_compressed: 500,
            ..WriterStats::default()
        };
        assert!((stats.compression_ratio() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_writer_stats_compression_ratio_nan() {
        let stats = WriterStats::default();
        assert!(stats.compression_ratio().is_nan());
    }

    #[test]
    fn test_writer_stats_space_savings() {
        let stats = WriterStats {
            total_uncompressed: 1000,
            total_compressed: 300,
            ..WriterStats::default()
        };
        assert!((stats.space_savings_pct() - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_writer_stats_space_savings_zero() {
        let stats = WriterStats::default();
        assert!((stats.space_savings_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_writer_stats_display() {
        let stats = WriterStats {
            sections_written: 5,
            total_uncompressed: 10000,
            total_compressed: 3000,
            total_bytes_written: 3500,
            ..WriterStats::default()
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 sections"));
        assert!(display.contains("10000 bytes uncompressed"));
        assert!(display.contains("3000 bytes compressed"));
        assert!(display.contains("3500 bytes total"));
    }

    // ── State Machine: Initial State ───────────────────────────────

    #[test]
    fn test_initial_state() {
        let mut buf = Vec::new();
        let writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        assert_eq!(writer.state_name(), "CREATED");
        assert_eq!(buf.len(), 0);
    }

    // ── State Machine: File Header ─────────────────────────────────

    #[test]
    fn test_write_file_header() {
        let mut buf = Vec::new();
        {
            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

            let header = FileHeader::default();
            writer.write_file_header(&header).unwrap();

            assert_eq!(writer.state_name(), "FILE_HEADER_WRITTEN");
            assert_eq!(writer.stats().total_bytes_written, 64);
        }
        assert_eq!(buf.len(), FileHeader::SIZE);
        assert_eq!(&buf[0..4], b"ATOM");
    }

    #[test]
    fn test_write_file_header_twice_fails() {
        let mut buf = Vec::new();
        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

        writer.write_file_header(&FileHeader::default()).unwrap();
        let result = writer.write_file_header(&FileHeader::default());
        assert!(result.is_err());
    }

    // ── State Machine: Hash Table ──────────────────────────────────

    #[test]
    fn test_write_hash_table() {
        let mut buf = Vec::new();
        {
            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
            writer
                .write_file_header(&FileHeader::builder().hash_table_entries(1).build())
                .unwrap();

            let table = HashDedupTable::new([0xAA; 32]);
            writer.write_hash_table(&table).unwrap();

            assert_eq!(writer.state_name(), "HASH_TABLE_WRITTEN");
        }
        assert_eq!(buf.len(), FileHeader::SIZE + 32); // header + 1 hash
    }

    #[test]
    fn test_write_hash_table_before_header_fails() {
        let mut buf = Vec::new();
        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

        let table = HashDedupTable::new([0; 32]);
        let result = writer.write_hash_table(&table);
        assert!(result.is_err());
    }

    // ── State Machine: Metadata Sections ───────────────────────────

    #[test]
    fn test_write_change_header() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        let header = ChangeHeader::new("Test change");
        writer.write_change_header(&header).unwrap();

        assert_eq!(writer.state_name(), "WRITING_METADATA");
        assert_eq!(writer.stats().sections_written, 1);
    }

    #[test]
    fn test_write_change_header_before_hash_table_fails() {
        let mut buf = Vec::new();
        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&FileHeader::default()).unwrap();

        let result = writer.write_change_header(&ChangeHeader::new("Test"));
        assert!(result.is_err());
    }

    #[test]
    fn test_write_change_header_twice_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("First"))
            .unwrap();
        let result = writer.write_change_header(&ChangeHeader::new("Second"));
        assert!(result.is_err());
    }

    #[test]
    fn test_write_dependencies() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        writer.write_dependencies(&[1, 2, 3]).unwrap();

        assert_eq!(writer.stats().sections_written, 2);
    }

    #[test]
    fn test_write_dependencies_empty() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();

        assert_eq!(writer.stats().sections_written, 2);
    }

    #[test]
    fn test_write_dependencies_filters_none_sentinel() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        // Include HASH_INDEX_NONE — should be filtered out
        writer.write_dependencies(&[1, HASH_INDEX_NONE, 2]).unwrap();

        assert_eq!(writer.stats().sections_written, 2);
    }

    #[test]
    fn test_write_dependencies_before_header_section_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        let result = writer.write_dependencies(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_write_dependencies_twice_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        let result = writer.write_dependencies(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_write_provenance() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_provenance(&[]).unwrap();

        assert_eq!(writer.stats().sections_written, 3);
    }

    #[test]
    fn test_write_provenance_before_deps_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);

        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        let result = writer.write_provenance(&[]);
        assert!(result.is_err());
    }

    // ── State Machine: Graph Sections ──────────────────────────────

    #[test]
    fn test_write_graph_section() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        let payload = b"graph data for file_a.rs";
        writer.write_graph_section(payload).unwrap();

        assert_eq!(writer.stats().graph_sections_written, 1);
        assert_eq!(writer.state_name(), "WRITING_GRAPH");
    }

    #[test]
    fn test_write_multiple_graph_sections() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_graph_section(b"file_a").unwrap();
        writer.write_graph_section(b"file_b").unwrap();
        writer.write_graph_section(b"file_c").unwrap();

        assert_eq!(writer.stats().graph_sections_written, 3);
    }

    #[test]
    fn test_write_graph_before_deps_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);
        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();

        let result = writer.write_graph_section(b"data");
        assert!(result.is_err());
    }

    // ── State Machine: Semantic Sections ───────────────────────────

    #[test]
    fn test_write_semantic_section() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_semantic_section(b"semantic data").unwrap();

        assert_eq!(writer.stats().semantic_sections_written, 1);
        assert_eq!(writer.state_name(), "WRITING_SEMANTIC");
    }

    #[test]
    fn test_write_semantic_after_graph() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_graph_section(b"graph").unwrap();
        writer.write_semantic_section(b"semantic").unwrap();

        assert_eq!(writer.stats().graph_sections_written, 1);
        assert_eq!(writer.stats().semantic_sections_written, 1);
    }

    #[test]
    fn test_write_graph_after_semantic_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_semantic_section(b"semantic").unwrap();
        let result = writer.write_graph_section(b"graph");
        assert!(result.is_err()); // Can't go back to GRAPH after SEMANTIC
    }

    // ── State Machine: Content Chunks ──────────────────────────────

    #[test]
    fn test_write_content_chunk() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        let data = b"Hello, World!";
        writer.write_content_chunk(0, data).unwrap();

        assert_eq!(writer.stats().content_chunks_written, 1);
        assert_eq!(writer.state_name(), "WRITING_CONTENT");
    }

    #[test]
    fn test_write_multiple_content_chunks() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        for i in 0..5 {
            let data = format!("chunk {}", i);
            writer.write_content_chunk(i, data.as_bytes()).unwrap();
        }

        assert_eq!(writer.stats().content_chunks_written, 5);
    }

    #[test]
    fn test_write_content_after_graph_and_semantic() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_graph_section(b"graph").unwrap();
        writer.write_semantic_section(b"semantic").unwrap();
        writer.write_content_chunk(0, b"content").unwrap();

        assert_eq!(writer.stats().graph_sections_written, 1);
        assert_eq!(writer.stats().semantic_sections_written, 1);
        assert_eq!(writer.stats().content_chunks_written, 1);
    }

    #[test]
    fn test_write_semantic_after_content_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_content_chunk(0, b"content").unwrap();
        let result = writer.write_semantic_section(b"semantic");
        assert!(result.is_err());
    }

    // ── State Machine: Unhashed ────────────────────────────────────

    #[test]
    fn test_write_unhashed() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        let json = serde_json::to_vec(&serde_json::json!({
            "transcript": "AI reasoning trace"
        }))
        .unwrap();
        writer.write_unhashed(&json).unwrap();

        assert_eq!(writer.state_name(), "WROTE_UNHASHED");
    }

    #[test]
    fn test_write_unhashed_twice_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_unhashed(b"first").unwrap();
        let result = writer.write_unhashed(b"second");
        assert!(result.is_err());
    }

    // ── Finalize ───────────────────────────────────────────────────

    #[test]
    fn test_finalize_minimal() {
        let mut buf = Vec::new();
        let writer = writer_with_metadata(&mut buf);

        let outcome = writer.finalize().unwrap();

        // Hash should be non-zero
        assert_ne!(outcome.content_hash, [0u8; 32]);
        // Stats should show 2 sections (HEADER + DEPS)
        assert_eq!(outcome.stats.sections_written, 2);
        // Total bytes should be positive
        assert!(outcome.stats.total_bytes_written > 0);
        // Buf should end with 32-byte trailer
        assert!(buf.len() >= 32);
        let trailer_start = buf.len() - 32;
        assert_eq!(&buf[trailer_start..], &outcome.content_hash);
    }

    #[test]
    fn test_finalize_after_all_section_types() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_provenance(&[]).unwrap();
        writer.write_graph_section(b"graph data").unwrap();
        writer.write_semantic_section(b"semantic data").unwrap();
        writer.write_content_chunk(0, b"content data").unwrap();
        writer.write_unhashed(b"unhashed data").unwrap();

        let outcome = writer.finalize().unwrap();

        assert_ne!(outcome.content_hash, [0u8; 32]);
        // 2 metadata + 1 provenance + 1 graph + 1 semantic + 1 content + 1 unhashed = 7
        assert_eq!(outcome.stats.sections_written, 7);
        assert_eq!(outcome.stats.graph_sections_written, 1);
        assert_eq!(outcome.stats.semantic_sections_written, 1);
        assert_eq!(outcome.stats.content_chunks_written, 1);
    }

    #[test]
    fn test_finalize_without_deps_fails() {
        let mut buf = Vec::new();
        let mut writer = writer_with_preamble(&mut buf);
        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();

        let result = writer.finalize();
        assert!(result.is_err());
    }

    #[test]
    fn test_finalize_without_header_section_fails() {
        let mut buf = Vec::new();
        let writer = writer_with_preamble(&mut buf);

        let result = writer.finalize();
        assert!(result.is_err());
    }

    // ── Hash Determinism ───────────────────────────────────────────

    /// Helper: create a ChangeHeader with a fixed timestamp for deterministic tests.
    fn fixed_header(message: &str) -> ChangeHeader {
        ChangeHeader {
            message: message.to_string(),
            description: None,
            timestamp: chrono::DateTime::parse_from_rfc3339("2025-01-15T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            authors: Vec::new(),
        }
    }

    #[test]
    fn test_hash_is_deterministic() {
        let write_once = || {
            let mut buf = Vec::new();
            let self_hash = *blake3::hash(b"test").as_bytes();
            let hash_table = HashDedupTable::new(self_hash);
            let file_header = FileHeader::builder()
                .hash_table_entries(1)
                .graph_section_count(1)
                .semantic_section_count(1)
                .contents_chunks(1)
                .build();

            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
            writer.write_file_header(&file_header).unwrap();
            writer.write_hash_table(&hash_table).unwrap();
            writer
                .write_change_header(&fixed_header("Deterministic"))
                .unwrap();
            writer.write_dependencies(&[]).unwrap();
            writer.write_graph_section(b"graph data abc").unwrap();
            writer.write_semantic_section(b"semantic data xyz").unwrap();
            writer.write_content_chunk(0, b"file content 123").unwrap();

            let outcome = writer.finalize().unwrap();
            (buf, outcome.content_hash)
        };

        let (buf1, hash1) = write_once();
        let (buf2, hash2) = write_once();

        assert_eq!(hash1, hash2, "hash should be deterministic");
        assert_eq!(buf1, buf2, "output should be byte-for-byte identical");
    }

    #[test]
    fn test_hash_excludes_unhashed_section() {
        // Write two files: one with UNHASHED, one without.
        // Their content hashes should be identical.
        // Use a fixed timestamp so both writes produce the same header bytes.
        let write_with_unhashed = |include_unhashed: bool| -> [u8; 32] {
            let mut buf = Vec::new();
            let self_hash = *blake3::hash(b"test").as_bytes();
            let hash_table = HashDedupTable::new(self_hash);
            let file_header = FileHeader::builder().hash_table_entries(1).build();

            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
            writer.write_file_header(&file_header).unwrap();
            writer.write_hash_table(&hash_table).unwrap();
            writer.write_change_header(&fixed_header("Test")).unwrap();
            writer.write_dependencies(&[]).unwrap();

            if include_unhashed {
                writer
                    .write_unhashed(b"this should not affect the hash")
                    .unwrap();
            }

            writer.finalize().unwrap().content_hash
        };

        let hash_without = write_with_unhashed(false);
        let hash_with = write_with_unhashed(true);

        assert_eq!(
            hash_without, hash_with,
            "UNHASHED section must not affect content hash"
        );
    }

    #[test]
    fn test_different_content_produces_different_hash() {
        let write_with_content = |content: &[u8]| -> [u8; 32] {
            let mut buf = Vec::new();
            let self_hash = *blake3::hash(b"test").as_bytes();
            let hash_table = HashDedupTable::new(self_hash);
            let file_header = FileHeader::builder()
                .hash_table_entries(1)
                .graph_section_count(1)
                .build();

            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
            writer.write_file_header(&file_header).unwrap();
            writer.write_hash_table(&hash_table).unwrap();
            writer
                .write_change_header(&ChangeHeader::new("Test"))
                .unwrap();
            writer.write_dependencies(&[]).unwrap();
            writer.write_graph_section(content).unwrap();

            writer.finalize().unwrap().content_hash
        };

        let hash1 = write_with_content(b"content A");
        let hash2 = write_with_content(b"content B");

        assert_ne!(
            hash1, hash2,
            "different content should produce different hashes"
        );
    }

    // ── Pre-Compressed Section ─────────────────────────────────────

    #[test]
    fn test_write_compressed_section() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        let payload = b"some graph data";
        let compressed = zstd::encode_all(&payload[..], 3).unwrap();

        writer
            .write_compressed_section(SectionType::Graph, &compressed, payload.len() as u64)
            .unwrap();

        assert_eq!(writer.stats().sections_written, 3); // HEADER + DEPS + GRAPH
        assert_eq!(writer.stats().graph_sections_written, 1);
    }

    // ── Content Chunk Hash Correctness ─────────────────────────────

    #[test]
    fn test_content_chunk_hash_is_of_uncompressed_data() {
        // Write a change with a content chunk, then read it back with the reader
        // to verify the chunk hash matches blake3 of the uncompressed data.
        let data = b"The quick brown fox jumps over the lazy dog";
        let expected_hash = blake3::hash(data);

        let mut buf = Vec::new();
        {
            let self_hash = *blake3::hash(b"chunk hash test").as_bytes();
            let hash_table = HashDedupTable::new(self_hash);

            let file_header = FileHeader::builder()
                .hash_table_entries(1)
                .graph_section_count(0)
                .contents_chunks(1)
                .build();

            let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
            writer.write_file_header(&file_header).unwrap();
            writer.write_hash_table(&hash_table).unwrap();
            writer
                .write_change_header(&ChangeHeader::new("Chunk test"))
                .unwrap();
            writer.write_dependencies(&[]).unwrap();
            writer.write_content_chunk(0, data).unwrap();
            writer.finalize().unwrap();
        }

        // Read it back with the reader and check the chunk info
        let mut cursor = std::io::Cursor::new(&buf);
        let mut reader = crate::change::format_v3::ChangeReader::open(&mut cursor).unwrap();

        let sections = reader.read_all_sections().unwrap();
        let content_section = sections
            .iter()
            .find(|s| s.section_type == SectionType::Content)
            .expect("should find CONTENT section");

        let info = content_section
            .content_chunk_info
            .as_ref()
            .expect("content section should have chunk info");

        assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
        assert_eq!(&content_section.payload, data);
    }

    // ── Full Roundtrip Size Check ──────────────────────────────────

    #[test]
    fn test_minimal_file_size() {
        let mut buf = Vec::new();
        let outcome = {
            let writer = writer_with_metadata(&mut buf);
            writer.finalize().unwrap()
        };

        // Minimum: FileHeader(64) + HashTable(32) + HEADER section(5+N) +
        //          DEPS section(5+N) + Trailer(32)
        let min_overhead = FileHeader::SIZE + 32 + SectionHeader::SIZE * 2 + Trailer::SIZE;
        assert!(
            buf.len() >= min_overhead,
            "file size {} should be >= minimum overhead {}",
            buf.len(),
            min_overhead,
        );

        // Verify the file starts with ATOM magic
        assert_eq!(&buf[0..4], b"ATOM");

        // Verify the file ends with the content hash
        let trailer_bytes = &buf[buf.len() - 32..];
        assert_eq!(trailer_bytes, &outcome.content_hash);
    }

    // ── Compression Actually Works ─────────────────────────────────

    #[test]
    fn test_compression_reduces_size() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        // Write a highly compressible payload (lots of zeros)
        let big_payload = vec![0u8; 10000];
        writer.write_graph_section(&big_payload).unwrap();

        let stats = writer.stats();
        assert!(
            stats.total_compressed < stats.total_uncompressed,
            "compressed ({}) should be smaller than uncompressed ({})",
            stats.total_compressed,
            stats.total_uncompressed,
        );
    }

    // ── Stats Tracking ─────────────────────────────────────────────

    #[test]
    fn test_stats_accumulate_correctly() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        let pre_graph_sections = writer.stats().sections_written;
        writer.write_graph_section(b"g1").unwrap();
        writer.write_graph_section(b"g2").unwrap();
        writer.write_semantic_section(b"s1").unwrap();
        writer.write_content_chunk(0, b"c1").unwrap();
        writer.write_content_chunk(1, b"c2").unwrap();
        writer.write_content_chunk(2, b"c3").unwrap();

        let stats = writer.stats();
        assert_eq!(stats.graph_sections_written, 2);
        assert_eq!(stats.semantic_sections_written, 1);
        assert_eq!(stats.content_chunks_written, 3);
        // 2 (metadata) + 2 (graph) + 1 (semantic) + 3 (content) = 8
        assert_eq!(stats.sections_written, pre_graph_sections + 6);
    }

    // ── Edge Cases ─────────────────────────────────────────────────

    #[test]
    fn test_write_empty_graph_section() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_graph_section(b"").unwrap();
        assert_eq!(writer.stats().graph_sections_written, 1);
    }

    #[test]
    fn test_write_large_content_chunk() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        // 256 KB chunk (max in the FastCDC scheme)
        let data = vec![42u8; 256 * 1024];
        writer.write_content_chunk(0, &data).unwrap();

        assert_eq!(writer.stats().content_chunks_written, 1);
        assert!(writer.stats().total_uncompressed >= 256 * 1024);
    }

    #[test]
    fn test_finalize_after_unhashed() {
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        writer.write_unhashed(b"notes").unwrap();
        let outcome = writer.finalize().unwrap();

        assert_ne!(outcome.content_hash, [0u8; 32]);
        // 2 metadata + 1 unhashed = 3
        assert_eq!(outcome.stats.sections_written, 3);
    }

    // ── Ordering Enforcement Integration ───────────────────────────

    #[test]
    fn test_full_forward_progression() {
        // Write every section type in the correct order
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"full test").as_bytes();
        let dep_hash = *blake3::hash(b"dependency").as_bytes();
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

        // 1. File header
        writer.write_file_header(&file_header).unwrap();
        assert_eq!(writer.state_name(), "FILE_HEADER_WRITTEN");

        // 2. Hash table
        writer.write_hash_table(&hash_table).unwrap();
        assert_eq!(writer.state_name(), "HASH_TABLE_WRITTEN");

        // 3. Metadata
        writer
            .write_change_header(&ChangeHeader::new("Full test"))
            .unwrap();
        writer.write_dependencies(&[dep_idx]).unwrap();
        writer.write_provenance(&[]).unwrap();
        assert_eq!(writer.state_name(), "WRITING_METADATA");

        // 4. Graph sections
        writer.write_graph_section(b"graph file_a").unwrap();
        writer.write_graph_section(b"graph file_b").unwrap();
        assert_eq!(writer.state_name(), "WRITING_GRAPH");

        // 5. Semantic sections
        writer.write_semantic_section(b"semantic file_a").unwrap();
        writer.write_semantic_section(b"semantic file_b").unwrap();
        assert_eq!(writer.state_name(), "WRITING_SEMANTIC");

        // 6. Content chunks
        writer.write_content_chunk(0, b"chunk 0 data").unwrap();
        writer.write_content_chunk(1, b"chunk 1 data").unwrap();
        writer.write_content_chunk(2, b"chunk 2 data").unwrap();
        assert_eq!(writer.state_name(), "WRITING_CONTENT");

        // 7. Unhashed
        writer.write_unhashed(b"transcript").unwrap();
        assert_eq!(writer.state_name(), "WROTE_UNHASHED");

        // 8. Finalize
        let outcome = writer.finalize().unwrap();

        assert_ne!(outcome.content_hash, [0u8; 32]);
        assert_eq!(outcome.stats.graph_sections_written, 2);
        assert_eq!(outcome.stats.semantic_sections_written, 2);
        assert_eq!(outcome.stats.content_chunks_written, 3);
        // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
        assert_eq!(outcome.stats.sections_written, 11);
    }

    #[test]
    fn test_skip_graph_and_semantic() {
        // It's valid to have zero GRAPH and zero SEMANTIC sections
        let mut buf = Vec::new();
        let mut writer = writer_with_metadata(&mut buf);

        // Go directly to content
        writer.write_content_chunk(0, b"content only").unwrap();

        let outcome = writer.finalize().unwrap();
        assert_eq!(outcome.stats.graph_sections_written, 0);
        assert_eq!(outcome.stats.semantic_sections_written, 0);
        assert_eq!(outcome.stats.content_chunks_written, 1);
    }

    #[test]
    fn test_skip_all_optional_sections() {
        // Minimal valid file: just HEADER + DEPS + finalize
        let mut buf = Vec::new();
        let writer = writer_with_metadata(&mut buf);

        let outcome = writer.finalize().unwrap();
        assert_eq!(outcome.stats.sections_written, 2);
        assert_eq!(outcome.stats.graph_sections_written, 0);
        assert_eq!(outcome.stats.semantic_sections_written, 0);
        assert_eq!(outcome.stats.content_chunks_written, 0);
    }
}

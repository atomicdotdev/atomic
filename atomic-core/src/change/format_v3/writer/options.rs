//! Writer configuration, statistics, and outcome types.
//!
//! [`WriterOptions`] controls compression level and other writer behavior.
//! [`WriterStats`] tracks statistics during the writing process.
//! [`WriteOutcome`] is returned by [`ChangeWriter::finalize`](super::ChangeWriter::finalize).

/// Default zstd compression level.
///
/// Level 3 provides a good balance between compression ratio and speed.
/// Higher levels (up to 22) compress more but are significantly slower.
/// Level 1 is fastest with moderate compression.
pub(super) const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

// ═══════════════════════════════════════════════════════════════════════
// WriterOptions — configuration for ChangeWriter
// ═══════════════════════════════════════════════════════════════════════

/// Configuration options for [`ChangeWriter`](super::ChangeWriter).
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
/// These are available after [`ChangeWriter::finalize`](super::ChangeWriter::finalize)
/// via the [`WriteOutcome`] return value, and are useful for logging,
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
/// Returned by [`ChangeWriter::finalize`](super::ChangeWriter::finalize).
/// Contains the computed content hash (which is the change's identity) and
/// writing statistics.
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

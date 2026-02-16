//! Content-defined chunking and parallel compression for V3 change files.
//!
//! This module implements the FastCDC (Fast Content-Defined Chunking) algorithm
//! for splitting content blobs into variable-size chunks with content-determined
//! boundaries. Combined with rayon parallel compression, this enables:
//!
//! - **Delta transfer**: Small edits only invalidate 1-2 chunks; unchanged chunks
//!   keep the same hash across changes, enabling efficient push/pull.
//! - **Cross-change deduplication**: Identical content regions (renames, copies,
//!   reverts) produce identical chunk hashes and are stored once.
//! - **Parallel compression**: Each chunk compresses independently with rayon,
//!   utilizing all CPU cores.
//! - **Bounded memory**: Only one chunk is in memory at a time during streaming.
//!
//! # Algorithm
//!
//! FastCDC uses a Gear rolling hash to find chunk boundaries in O(n) time.
//! The boundaries are determined by the **content itself**, not by fixed offsets.
//! This means a small edit in the middle of a file only changes the chunk
//! containing the edit — all other chunks remain identical.
//!
//! # Chunk Size Targets
//!
//! | Parameter | Value | Rationale |
//! |-----------|-------|-----------|
//! | Minimum | 16 KB | Avoid excessive fragmentation / overhead |
//! | Average | 64 KB | Good balance of dedup granularity vs overhead |
//! | Maximum | 256 KB | Bound worst-case chunk size for memory |
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::format_v3::chunking::{
//!     chunk_content, ChunkingOptions, ContentChunk, compress_chunks_parallel,
//! };
//!
//! let data = b"Hello, World! ".repeat(10000);
//! let chunks = chunk_content(&data, &ChunkingOptions::default());
//!
//! assert!(!chunks.is_empty());
//! for chunk in &chunks {
//!     assert!(chunk.length >= ChunkingOptions::default().min_size);
//!     assert!(chunk.length <= ChunkingOptions::default().max_size);
//! }
//!
//! // Parallel compression
//! let compressed = compress_chunks_parallel(&data, &chunks, 3);
//! assert_eq!(compressed.len(), chunks.len());
//! ```
//!
//! # Thread Safety
//!
//! [`chunk_content`] is a pure function (no shared state). [`compress_chunks_parallel`]
//! uses rayon internally and is safe to call from any thread.

use rayon::prelude::*;
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// ChunkingOptions — configurable parameters for FastCDC
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for content-defined chunking.
///
/// These parameters control the FastCDC algorithm's chunk size distribution.
/// The defaults match the values recommended in the proposal:
///
/// - Minimum: 16 KB (avoid fragmentation)
/// - Average: 64 KB (good dedup granularity)
/// - Maximum: 256 KB (bounded memory)
///
/// # Customization
///
/// Smaller average sizes increase dedup effectiveness but also increase
/// per-chunk overhead (each chunk has a 45-byte header + blake3 hash).
/// Larger sizes reduce overhead but decrease dedup granularity.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::ChunkingOptions;
///
/// // Default: 16 KB min, 64 KB avg, 256 KB max
/// let opts = ChunkingOptions::default();
/// assert_eq!(opts.min_size, 16 * 1024);
/// assert_eq!(opts.avg_size, 64 * 1024);
/// assert_eq!(opts.max_size, 256 * 1024);
///
/// // Smaller chunks for better dedup (at the cost of more overhead)
/// let small = ChunkingOptions::new(8 * 1024, 32 * 1024, 128 * 1024);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkingOptions {
    /// Minimum chunk size in bytes. Chunks will never be smaller than this
    /// unless the remaining data is shorter.
    pub min_size: usize,

    /// Target average chunk size in bytes. The rolling hash parameters are
    /// tuned to produce chunks of approximately this size on average.
    pub avg_size: usize,

    /// Maximum chunk size in bytes. If no boundary is found within this
    /// many bytes, a forced boundary is inserted.
    pub max_size: usize,
}

impl ChunkingOptions {
    /// Create custom chunking options.
    ///
    /// # Arguments
    ///
    /// * `min_size` - Minimum chunk size (bytes).
    /// * `avg_size` - Target average chunk size (bytes).
    /// * `max_size` - Maximum chunk size (bytes).
    ///
    /// # Panics
    ///
    /// Panics if `min_size > avg_size` or `avg_size > max_size`.
    pub fn new(min_size: usize, avg_size: usize, max_size: usize) -> Self {
        assert!(
            min_size <= avg_size,
            "min_size ({}) must be <= avg_size ({})",
            min_size,
            avg_size
        );
        assert!(
            avg_size <= max_size,
            "avg_size ({}) must be <= max_size ({})",
            avg_size,
            max_size
        );
        Self {
            min_size,
            avg_size,
            max_size,
        }
    }

    /// Options tuned for small content (< 1 MB).
    ///
    /// Uses smaller chunk sizes to ensure at least a few chunks are produced,
    /// improving dedup effectiveness for small files.
    pub fn small() -> Self {
        Self {
            min_size: 4 * 1024,
            avg_size: 16 * 1024,
            max_size: 64 * 1024,
        }
    }

    /// Options tuned for large content (> 10 MB).
    ///
    /// Uses larger chunk sizes to reduce per-chunk overhead when dealing
    /// with large binary files or repositories.
    pub fn large() -> Self {
        Self {
            min_size: 64 * 1024,
            avg_size: 256 * 1024,
            max_size: 1024 * 1024,
        }
    }
}

impl Default for ChunkingOptions {
    /// Default options: 16 KB min, 64 KB avg, 256 KB max.
    ///
    /// These values match the proposal's recommendations for a good balance
    /// of dedup effectiveness vs per-chunk overhead.
    fn default() -> Self {
        Self {
            min_size: 16 * 1024,
            avg_size: 64 * 1024,
            max_size: 256 * 1024,
        }
    }
}

impl fmt::Display for ChunkingOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ChunkingOptions(min={}, avg={}, max={})",
            format_size(self.min_size),
            format_size(self.avg_size),
            format_size(self.max_size),
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ContentChunk — metadata about a single chunk
// ═══════════════════════════════════════════════════════════════════════

/// Metadata about a single content chunk produced by [`chunk_content`].
///
/// This struct describes **where** a chunk is in the source data and its
/// blake3 hash. The actual chunk data is a slice of the original content
/// at `data[offset..offset+length]`.
///
/// # Content Addressing
///
/// The `hash` field is the blake3 hash of the **uncompressed** chunk data.
/// Two chunks with identical content will have identical hashes regardless
/// of which change or file they came from. This is the foundation of
/// delta transfer and cross-change deduplication.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::{chunk_content, ChunkingOptions};
///
/// let data = vec![0u8; 100_000]; // 100 KB of zeros
/// let chunks = chunk_content(&data, &ChunkingOptions::default());
///
/// for chunk in &chunks {
///     // Verify the hash matches the chunk data
///     let chunk_data = &data[chunk.offset..chunk.offset + chunk.length];
///     let expected_hash = *blake3::hash(chunk_data).as_bytes();
///     assert_eq!(chunk.hash, expected_hash);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentChunk {
    /// Byte offset of this chunk within the source data.
    pub offset: usize,

    /// Length of this chunk in bytes.
    pub length: usize,

    /// Blake3 hash of the uncompressed chunk data.
    ///
    /// This is the content address — identical content produces identical
    /// hashes, enabling deduplication across changes.
    pub hash: [u8; 32],

    /// Sequential index of this chunk (0-based).
    pub index: u32,
}

impl ContentChunk {
    /// Returns the exclusive end offset of this chunk: `offset + length`.
    #[inline]
    pub fn end(&self) -> usize {
        self.offset + self.length
    }

    /// Returns `true` if this chunk has zero length.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Get the chunk data from a source buffer.
    ///
    /// # Panics
    ///
    /// Panics if the chunk's range exceeds the source buffer length.
    #[inline]
    pub fn data<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        &source[self.offset..self.end()]
    }
}

impl fmt::Display for ContentChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Chunk#{} [{}..{}) {} ({:02x}{:02x}{:02x}{:02x}…)",
            self.index,
            self.offset,
            self.end(),
            format_size(self.length),
            self.hash[0],
            self.hash[1],
            self.hash[2],
            self.hash[3],
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompressedChunk — a chunk after zstd compression
// ═══════════════════════════════════════════════════════════════════════

/// A content chunk after zstd compression.
///
/// Produced by [`compress_chunks_parallel`]. Contains the compressed data
/// and the original chunk's metadata (offset, length, hash, index).
///
/// # Size Comparison
///
/// For typical source code, zstd achieves 3-5x compression. The
/// `compression_ratio()` method reports the actual ratio.
#[derive(Clone, Debug)]
pub struct CompressedChunk {
    /// The original chunk metadata (offset, length, hash, index).
    pub chunk: ContentChunk,

    /// The zstd-compressed data.
    pub compressed_data: Vec<u8>,
}

impl CompressedChunk {
    /// Returns the uncompressed size in bytes.
    #[inline]
    pub fn uncompressed_len(&self) -> usize {
        self.chunk.length
    }

    /// Returns the compressed size in bytes.
    #[inline]
    pub fn compressed_len(&self) -> usize {
        self.compressed_data.len()
    }

    /// Returns the compression ratio (compressed / uncompressed).
    ///
    /// Returns `f64::NAN` if the uncompressed length is zero.
    pub fn compression_ratio(&self) -> f64 {
        if self.chunk.length == 0 {
            return f64::NAN;
        }
        self.compressed_data.len() as f64 / self.chunk.length as f64
    }

    /// Returns the space savings as a percentage (0.0 to 100.0).
    pub fn space_savings_pct(&self) -> f64 {
        if self.chunk.length == 0 {
            return 0.0;
        }
        (1.0 - self.compressed_data.len() as f64 / self.chunk.length as f64) * 100.0
    }
}

impl fmt::Display for CompressedChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CompressedChunk#{} {} → {} ({:.1}% savings)",
            self.chunk.index,
            format_size(self.chunk.length),
            format_size(self.compressed_data.len()),
            self.space_savings_pct(),
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ChunkingResult — summary of chunking operation
// ═══════════════════════════════════════════════════════════════════════

/// Summary statistics from a chunking operation.
///
/// Returned by [`chunk_content_with_stats`] for logging and progress reporting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkingStats {
    /// Number of chunks produced.
    pub chunk_count: usize,

    /// Total input size in bytes.
    pub total_input_bytes: usize,

    /// Smallest chunk size in bytes.
    pub min_chunk_size: usize,

    /// Largest chunk size in bytes.
    pub max_chunk_size: usize,

    /// Average chunk size in bytes.
    pub avg_chunk_size: usize,
}

impl fmt::Display for ChunkingStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} chunks from {} input (min={}, avg={}, max={})",
            self.chunk_count,
            format_size(self.total_input_bytes),
            format_size(self.min_chunk_size),
            format_size(self.avg_chunk_size),
            format_size(self.max_chunk_size),
        )
    }
}

/// Summary statistics from a parallel compression operation.
///
/// Returned by [`compress_chunks_parallel_with_stats`].
#[derive(Clone, Debug, Default)]
pub struct CompressionStats {
    /// Number of chunks compressed.
    pub chunk_count: usize,

    /// Total uncompressed size in bytes.
    pub total_uncompressed: usize,

    /// Total compressed size in bytes.
    pub total_compressed: usize,

    /// Compression level used.
    pub compression_level: i32,
}

impl CompressionStats {
    /// Overall compression ratio (compressed / uncompressed).
    pub fn compression_ratio(&self) -> f64 {
        if self.total_uncompressed == 0 {
            return f64::NAN;
        }
        self.total_compressed as f64 / self.total_uncompressed as f64
    }

    /// Overall space savings as a percentage.
    pub fn space_savings_pct(&self) -> f64 {
        if self.total_uncompressed == 0 {
            return 0.0;
        }
        (1.0 - self.total_compressed as f64 / self.total_uncompressed as f64) * 100.0
    }
}

impl fmt::Display for CompressionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} chunks, {} → {} ({:.1}% savings, level {})",
            self.chunk_count,
            format_size(self.total_uncompressed),
            format_size(self.total_compressed),
            self.space_savings_pct(),
            self.compression_level,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// chunk_content — the main chunking function
// ═══════════════════════════════════════════════════════════════════════

/// Split content into variable-size chunks using FastCDC.
///
/// Returns a list of [`ContentChunk`] descriptors. Each chunk's data can be
/// accessed via `&data[chunk.offset..chunk.offset + chunk.length]` or
/// via [`ContentChunk::data`].
///
/// # Small Data Handling
///
/// If the input data is smaller than `options.min_size`, a single chunk
/// is returned covering the entire input. If the input is empty, an
/// empty vector is returned.
///
/// # Algorithm
///
/// Uses the `fastcdc` crate's streaming chunker with Gear hash. Chunk
/// boundaries are determined by the content itself (rolling hash hits a
/// target), not by fixed offsets. This means a small edit in the middle
/// of a file only affects 1-2 chunks — all other chunks keep the same
/// boundaries and the same hashes.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::{chunk_content, ChunkingOptions};
///
/// let data = vec![42u8; 200_000]; // 200 KB
/// let chunks = chunk_content(&data, &ChunkingOptions::default());
///
/// // Should produce ~3 chunks (200KB / 64KB avg ≈ 3)
/// assert!(chunks.len() >= 1);
///
/// // Verify chunks cover the entire input
/// let total: usize = chunks.iter().map(|c| c.length).sum();
/// assert_eq!(total, data.len());
///
/// // Verify chunks are contiguous
/// for i in 1..chunks.len() {
///     assert_eq!(chunks[i].offset, chunks[i-1].offset + chunks[i-1].length);
/// }
/// ```
pub fn chunk_content(data: &[u8], options: &ChunkingOptions) -> Vec<ContentChunk> {
    if data.is_empty() {
        return Vec::new();
    }

    // For data smaller than min_size, return a single chunk
    if data.len() <= options.min_size {
        return vec![ContentChunk {
            offset: 0,
            length: data.len(),
            hash: *blake3::hash(data).as_bytes(),
            index: 0,
        }];
    }

    let chunker = fastcdc::v2020::FastCDC::new(
        data,
        options.min_size as u32,
        options.avg_size as u32,
        options.max_size as u32,
    );

    chunker
        .enumerate()
        .map(|(i, chunk)| {
            let chunk_data = &data[chunk.offset..chunk.offset + chunk.length];
            ContentChunk {
                offset: chunk.offset,
                length: chunk.length,
                hash: *blake3::hash(chunk_data).as_bytes(),
                index: i as u32,
            }
        })
        .collect()
}

/// Split content into chunks and return chunking statistics.
///
/// This is the same as [`chunk_content`] but also returns [`ChunkingStats`]
/// for logging and progress reporting.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::{chunk_content_with_stats, ChunkingOptions};
///
/// let data = vec![0u8; 500_000]; // 500 KB
/// let (chunks, stats) = chunk_content_with_stats(&data, &ChunkingOptions::default());
///
/// println!("{}", stats);
/// assert_eq!(stats.total_input_bytes, 500_000);
/// assert!(stats.chunk_count >= 1);
/// ```
pub fn chunk_content_with_stats(
    data: &[u8],
    options: &ChunkingOptions,
) -> (Vec<ContentChunk>, ChunkingStats) {
    let chunks = chunk_content(data, options);

    let stats = if chunks.is_empty() {
        ChunkingStats::default()
    } else {
        let sizes: Vec<usize> = chunks.iter().map(|c| c.length).collect();
        let min_size = *sizes.iter().min().unwrap_or(&0);
        let max_size = *sizes.iter().max().unwrap_or(&0);
        let total: usize = sizes.iter().sum();
        let avg_size = total / sizes.len();

        ChunkingStats {
            chunk_count: chunks.len(),
            total_input_bytes: data.len(),
            min_chunk_size: min_size,
            max_chunk_size: max_size,
            avg_chunk_size: avg_size,
        }
    };

    (chunks, stats)
}

// ═══════════════════════════════════════════════════════════════════════
// compress_chunks_parallel — parallel zstd compression with rayon
// ═══════════════════════════════════════════════════════════════════════

/// Compress all chunks in parallel using rayon and zstd.
///
/// Each chunk is compressed independently on a rayon worker thread. This
/// utilizes all available CPU cores and is the primary performance win
/// for large changes with many chunks.
///
/// # Arguments
///
/// * `data` - The full source data buffer (chunks index into this).
/// * `chunks` - The chunk descriptors from [`chunk_content`].
/// * `compression_level` - Zstd compression level (1-22, typically 3).
///
/// # Returns
///
/// A vector of [`CompressedChunk`] in the same order as the input chunks.
///
/// # Panics
///
/// Panics if any chunk's range exceeds the source data length, or if
/// zstd compression fails (which should only happen on out-of-memory).
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::{
///     chunk_content, compress_chunks_parallel, ChunkingOptions,
/// };
///
/// let data = vec![42u8; 200_000];
/// let chunks = chunk_content(&data, &ChunkingOptions::default());
/// let compressed = compress_chunks_parallel(&data, &chunks, 3);
///
/// for cc in &compressed {
///     // Compressed should be smaller than uncompressed for repetitive data
///     assert!(cc.compressed_len() <= cc.uncompressed_len() + 100);
/// }
/// ```
pub fn compress_chunks_parallel(
    data: &[u8],
    chunks: &[ContentChunk],
    compression_level: i32,
) -> Vec<CompressedChunk> {
    chunks
        .par_iter()
        .map(|chunk| {
            let chunk_data = chunk.data(data);
            let compressed_data = zstd::encode_all(chunk_data, compression_level)
                .expect("zstd compression should not fail");
            CompressedChunk {
                chunk: *chunk,
                compressed_data,
            }
        })
        .collect()
}

/// Compress all chunks in parallel and return compression statistics.
///
/// This is the same as [`compress_chunks_parallel`] but also returns
/// [`CompressionStats`] for logging and progress reporting.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::chunking::{
///     chunk_content, compress_chunks_parallel_with_stats, ChunkingOptions,
/// };
///
/// let data = b"Hello, World! ".repeat(10000);
/// let chunks = chunk_content(&data, &ChunkingOptions::default());
/// let (compressed, stats) = compress_chunks_parallel_with_stats(&data, &chunks, 3);
///
/// println!("{}", stats);
/// assert!(stats.total_compressed < stats.total_uncompressed);
/// ```
pub fn compress_chunks_parallel_with_stats(
    data: &[u8],
    chunks: &[ContentChunk],
    compression_level: i32,
) -> (Vec<CompressedChunk>, CompressionStats) {
    let compressed = compress_chunks_parallel(data, chunks, compression_level);

    let total_uncompressed: usize = compressed.iter().map(|c| c.uncompressed_len()).sum();
    let total_compressed: usize = compressed.iter().map(|c| c.compressed_len()).sum();

    let stats = CompressionStats {
        chunk_count: compressed.len(),
        total_uncompressed,
        total_compressed,
        compression_level,
    };

    (compressed, stats)
}

// ═══════════════════════════════════════════════════════════════════════
// Formatting helper
// ═══════════════════════════════════════════════════════════════════════

/// Format a byte size as a human-readable string (e.g., "64 KB", "1.2 MB").
fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── ChunkingOptions ────────────────────────────────────────────

    #[test]
    fn test_default_options() {
        let opts = ChunkingOptions::default();
        assert_eq!(opts.min_size, 16 * 1024);
        assert_eq!(opts.avg_size, 64 * 1024);
        assert_eq!(opts.max_size, 256 * 1024);
    }

    #[test]
    fn test_small_options() {
        let opts = ChunkingOptions::small();
        assert!(opts.min_size < ChunkingOptions::default().min_size);
        assert!(opts.avg_size < ChunkingOptions::default().avg_size);
    }

    #[test]
    fn test_large_options() {
        let opts = ChunkingOptions::large();
        assert!(opts.min_size > ChunkingOptions::default().min_size);
        assert!(opts.avg_size > ChunkingOptions::default().avg_size);
    }

    #[test]
    fn test_custom_options() {
        let opts = ChunkingOptions::new(1024, 4096, 16384);
        assert_eq!(opts.min_size, 1024);
        assert_eq!(opts.avg_size, 4096);
        assert_eq!(opts.max_size, 16384);
    }

    #[test]
    #[should_panic(expected = "min_size")]
    fn test_options_min_gt_avg_panics() {
        ChunkingOptions::new(100, 50, 200);
    }

    #[test]
    #[should_panic(expected = "avg_size")]
    fn test_options_avg_gt_max_panics() {
        ChunkingOptions::new(50, 200, 100);
    }

    #[test]
    fn test_options_display() {
        let opts = ChunkingOptions::default();
        let display = format!("{}", opts);
        assert!(display.contains("16.0 KB"));
        assert!(display.contains("64.0 KB"));
        assert!(display.contains("256.0 KB"));
    }

    // ── chunk_content: empty / small input ─────────────────────────

    #[test]
    fn test_chunk_empty_data() {
        let chunks = chunk_content(&[], &ChunkingOptions::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_tiny_data() {
        let data = b"Hello";
        let chunks = chunk_content(data, &ChunkingOptions::default());

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].length, 5);
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].hash, *blake3::hash(data).as_bytes());
    }

    #[test]
    fn test_chunk_exactly_min_size() {
        let data = vec![0u8; 16 * 1024]; // Exactly min_size
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].length, 16 * 1024);
    }

    #[test]
    fn test_chunk_one_byte_over_min() {
        let data = vec![42u8; 16 * 1024 + 1];
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        // Might be 1 chunk (if no boundary found within max_size)
        // or more — but must cover all data
        let total: usize = chunks.iter().map(|c| c.length).sum();
        assert_eq!(total, data.len());
    }

    // ── chunk_content: larger data ─────────────────────────────────

    #[test]
    fn test_chunk_medium_data() {
        // 500 KB of varied data — should produce multiple chunks
        let data: Vec<u8> = (0..500_000).map(|i| (i % 251) as u8).collect();
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        // 500KB / 64KB avg ≈ 7-8 chunks
        assert!(chunks.len() >= 2, "should produce multiple chunks");
        assert!(chunks.len() <= 50, "shouldn't produce too many chunks");

        // Verify contiguous coverage
        verify_chunks_cover_data(&data, &chunks);
    }

    #[test]
    fn test_chunk_large_data() {
        // 2 MB of pseudo-random data
        let data: Vec<u8> = (0..2_000_000u64)
            .map(|i| {
                // Simple PRNG for deterministic "random" data
                ((i.wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407))
                    >> 33) as u8
            })
            .collect();
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        // 2MB / 64KB avg ≈ 31 chunks
        assert!(chunks.len() >= 5, "2MB should produce at least 5 chunks");

        verify_chunks_cover_data(&data, &chunks);
    }

    #[test]
    fn test_chunk_highly_repetitive_data() {
        // Highly repetitive data — tests the rolling hash behavior
        let data = vec![0xAA; 300_000]; // 300 KB of 0xAA
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        verify_chunks_cover_data(&data, &chunks);
    }

    #[test]
    fn test_chunk_source_code_like_data() {
        // Simulate source code: lines of varying length
        let mut data = Vec::new();
        for i in 0..10_000 {
            let line = format!("    let x_{} = compute_value({}, {});\n", i, i * 7, i * 13);
            data.extend_from_slice(line.as_bytes());
        }
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        assert!(!chunks.is_empty());
        verify_chunks_cover_data(&data, &chunks);
    }

    // ── chunk_content: chunk properties ────────────────────────────

    #[test]
    fn test_chunk_indices_are_sequential() {
        let data = vec![42u8; 500_000];
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i as u32, "chunk index should be sequential");
        }
    }

    #[test]
    fn test_chunk_hashes_are_correct() {
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        for chunk in &chunks {
            let chunk_data = chunk.data(&data);
            let expected_hash = *blake3::hash(chunk_data).as_bytes();
            assert_eq!(
                chunk.hash, expected_hash,
                "chunk hash mismatch at index {}",
                chunk.index
            );
        }
    }

    #[test]
    fn test_chunk_sizes_respect_bounds() {
        let data: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
        let opts = ChunkingOptions::default();
        let chunks = chunk_content(&data, &opts);

        for chunk in &chunks {
            // All chunks except possibly the last should respect min_size
            if chunk.index < (chunks.len() - 1) as u32 {
                assert!(
                    chunk.length >= opts.min_size,
                    "chunk {} has length {} < min_size {}",
                    chunk.index,
                    chunk.length,
                    opts.min_size,
                );
            }
            // All chunks should respect max_size
            assert!(
                chunk.length <= opts.max_size,
                "chunk {} has length {} > max_size {}",
                chunk.index,
                chunk.length,
                opts.max_size,
            );
        }
    }

    // ── chunk_content: determinism ─────────────────────────────────

    #[test]
    fn test_chunk_deterministic() {
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let opts = ChunkingOptions::default();

        let chunks1 = chunk_content(&data, &opts);
        let chunks2 = chunk_content(&data, &opts);

        assert_eq!(chunks1.len(), chunks2.len());
        for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(c1, c2);
        }
    }

    #[test]
    fn test_chunk_stability_on_small_edit() {
        // The key property of content-defined chunking:
        // A small edit should only affect 1-2 chunks.
        let original: Vec<u8> = (0..500_000).map(|i| (i % 251) as u8).collect();
        let mut edited = original.clone();

        // Edit a few bytes in the middle
        let edit_pos = 250_000;
        edited[edit_pos] = 0xFF;
        edited[edit_pos + 1] = 0xFF;
        edited[edit_pos + 2] = 0xFF;

        let opts = ChunkingOptions::default();
        let orig_chunks = chunk_content(&original, &opts);
        let edit_chunks = chunk_content(&edited, &opts);

        // Count how many chunks have different hashes
        let orig_hashes: std::collections::HashSet<[u8; 32]> =
            orig_chunks.iter().map(|c| c.hash).collect();
        let edit_hashes: std::collections::HashSet<[u8; 32]> =
            edit_chunks.iter().map(|c| c.hash).collect();

        let unchanged_count = orig_hashes.intersection(&edit_hashes).count();
        let total_chunks = orig_chunks.len().max(edit_chunks.len());

        // At least 40% of chunks should be unchanged (typically >80%)
        // The exact percentage depends on chunk boundaries relative to the edit position.
        let unchanged_pct = unchanged_count as f64 / total_chunks as f64 * 100.0;
        assert!(
            unchanged_pct >= 40.0,
            "only {:.1}% of chunks unchanged after small edit (expected >=40%)",
            unchanged_pct,
        );
    }

    // ── chunk_content_with_stats ───────────────────────────────────

    #[test]
    fn test_chunk_with_stats_empty() {
        let (chunks, stats) = chunk_content_with_stats(&[], &ChunkingOptions::default());
        assert!(chunks.is_empty());
        assert_eq!(stats.chunk_count, 0);
    }

    #[test]
    fn test_chunk_with_stats_values() {
        let data = vec![42u8; 500_000];
        let (chunks, stats) = chunk_content_with_stats(&data, &ChunkingOptions::default());

        assert_eq!(stats.chunk_count, chunks.len());
        assert_eq!(stats.total_input_bytes, 500_000);
        assert!(stats.min_chunk_size > 0);
        assert!(stats.max_chunk_size >= stats.min_chunk_size);
        assert!(stats.avg_chunk_size >= stats.min_chunk_size);
        assert!(stats.avg_chunk_size <= stats.max_chunk_size);
    }

    #[test]
    fn test_chunk_stats_display() {
        let data = vec![42u8; 200_000];
        let (_, stats) = chunk_content_with_stats(&data, &ChunkingOptions::default());
        let display = format!("{}", stats);
        assert!(display.contains("chunks"));
        assert!(display.contains("input"));
    }

    // ── ContentChunk ───────────────────────────────────────────────

    #[test]
    fn test_content_chunk_end() {
        let chunk = ContentChunk {
            offset: 100,
            length: 50,
            hash: [0; 32],
            index: 0,
        };
        assert_eq!(chunk.end(), 150);
    }

    #[test]
    fn test_content_chunk_is_empty() {
        let empty = ContentChunk {
            offset: 0,
            length: 0,
            hash: [0; 32],
            index: 0,
        };
        assert!(empty.is_empty());

        let non_empty = ContentChunk {
            offset: 0,
            length: 10,
            hash: [0; 32],
            index: 0,
        };
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn test_content_chunk_data() {
        let source = b"Hello, World!";
        let chunk = ContentChunk {
            offset: 7,
            length: 6,
            hash: [0; 32],
            index: 0,
        };
        assert_eq!(chunk.data(source), b"World!");
    }

    #[test]
    fn test_content_chunk_display() {
        let chunk = ContentChunk {
            offset: 0,
            length: 65536,
            hash: [
                0xAB, 0xCD, 0xEF, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0,
            ],
            index: 3,
        };
        let display = format!("{}", chunk);
        assert!(display.contains("Chunk#3"));
        assert!(display.contains("64.0 KB"));
        assert!(display.contains("abcdef01"));
    }

    // ── compress_chunks_parallel ────────────────────────────────────

    #[test]
    fn test_compress_empty_chunks() {
        let compressed = compress_chunks_parallel(&[], &[], 3);
        assert!(compressed.is_empty());
    }

    #[test]
    fn test_compress_single_chunk() {
        let data = b"Hello, World! This is a test of compression.";
        let chunks = vec![ContentChunk {
            offset: 0,
            length: data.len(),
            hash: *blake3::hash(data).as_bytes(),
            index: 0,
        }];

        let compressed = compress_chunks_parallel(data, &chunks, 3);
        assert_eq!(compressed.len(), 1);
        assert_eq!(compressed[0].chunk.index, 0);
        assert!(compressed[0].compressed_len() > 0);
    }

    #[test]
    fn test_compress_multiple_chunks() {
        let data = vec![42u8; 200_000];
        let chunks = chunk_content(&data, &ChunkingOptions::default());
        let compressed = compress_chunks_parallel(&data, &chunks, 3);

        assert_eq!(compressed.len(), chunks.len());

        // Verify order is preserved
        for (i, cc) in compressed.iter().enumerate() {
            assert_eq!(cc.chunk.index, i as u32);
        }
    }

    #[test]
    fn test_compress_reduces_size_for_repetitive_data() {
        let data = vec![0u8; 300_000]; // Very compressible
        let chunks = chunk_content(&data, &ChunkingOptions::default());
        let compressed = compress_chunks_parallel(&data, &chunks, 3);

        let total_compressed: usize = compressed.iter().map(|c| c.compressed_len()).sum();
        let total_uncompressed: usize = compressed.iter().map(|c| c.uncompressed_len()).sum();

        assert!(
            total_compressed < total_uncompressed / 2,
            "compressed ({}) should be less than half of uncompressed ({})",
            total_compressed,
            total_uncompressed,
        );
    }

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let chunks = chunk_content(&data, &ChunkingOptions::default());
        let compressed = compress_chunks_parallel(&data, &chunks, 3);

        // Decompress each chunk and verify it matches the original
        for cc in &compressed {
            let decompressed = zstd::decode_all(&cc.compressed_data[..]).unwrap();
            let original_chunk_data = cc.chunk.data(&data);
            assert_eq!(
                decompressed, original_chunk_data,
                "decompressed chunk {} doesn't match original",
                cc.chunk.index,
            );
        }
    }

    #[test]
    fn test_compress_with_stats() {
        let data = b"Hello, World! ".repeat(10000);
        let chunks = chunk_content(&data, &ChunkingOptions::default());
        let (compressed, stats) = compress_chunks_parallel_with_stats(&data, &chunks, 3);

        assert_eq!(stats.chunk_count, compressed.len());
        assert_eq!(stats.compression_level, 3);
        assert!(stats.total_compressed < stats.total_uncompressed);
        assert!(stats.space_savings_pct() > 0.0);
    }

    #[test]
    fn test_compress_stats_display() {
        let stats = CompressionStats {
            chunk_count: 5,
            total_uncompressed: 100_000,
            total_compressed: 30_000,
            compression_level: 3,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 chunks"));
        assert!(display.contains("level 3"));
    }

    // ── CompressedChunk ────────────────────────────────────────────

    #[test]
    fn test_compressed_chunk_metrics() {
        let cc = CompressedChunk {
            chunk: ContentChunk {
                offset: 0,
                length: 1000,
                hash: [0; 32],
                index: 0,
            },
            compressed_data: vec![0; 300],
        };

        assert_eq!(cc.uncompressed_len(), 1000);
        assert_eq!(cc.compressed_len(), 300);
        assert!((cc.compression_ratio() - 0.3).abs() < f64::EPSILON);
        assert!((cc.space_savings_pct() - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compressed_chunk_zero_length() {
        let cc = CompressedChunk {
            chunk: ContentChunk {
                offset: 0,
                length: 0,
                hash: [0; 32],
                index: 0,
            },
            compressed_data: vec![],
        };

        assert!(cc.compression_ratio().is_nan());
        assert!((cc.space_savings_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compressed_chunk_display() {
        let cc = CompressedChunk {
            chunk: ContentChunk {
                offset: 0,
                length: 65536,
                hash: [0; 32],
                index: 2,
            },
            compressed_data: vec![0; 20000],
        };
        let display = format!("{}", cc);
        assert!(display.contains("CompressedChunk#2"));
        assert!(display.contains("savings"));
    }

    // ── Integration: chunk → compress → decompress ─────────────────

    #[test]
    fn test_full_pipeline() {
        // Simulate a realistic scenario: source code content
        let mut data = Vec::new();
        for i in 0..5000 {
            let line = format!(
                "/// Documentation for function {}\nfn func_{}(x: i32) -> i32 {{ x + {} }}\n\n",
                i,
                i,
                i * 7
            );
            data.extend_from_slice(line.as_bytes());
        }

        // Chunk
        let (chunks, chunk_stats) = chunk_content_with_stats(&data, &ChunkingOptions::default());
        assert!(chunk_stats.chunk_count >= 1);
        assert_eq!(chunk_stats.total_input_bytes, data.len());

        // Compress in parallel
        let (compressed, comp_stats) = compress_chunks_parallel_with_stats(&data, &chunks, 3);
        assert_eq!(comp_stats.chunk_count, chunks.len());
        assert!(comp_stats.total_compressed < comp_stats.total_uncompressed);

        // Decompress and reassemble
        let mut reassembled = Vec::with_capacity(data.len());
        for cc in &compressed {
            let decompressed = zstd::decode_all(&cc.compressed_data[..]).unwrap();
            reassembled.extend_from_slice(&decompressed);
        }

        assert_eq!(reassembled, data, "reassembled data should match original");
    }

    #[test]
    fn test_full_pipeline_with_different_compression_levels() {
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let chunks = chunk_content(&data, &ChunkingOptions::default());

        for level in [1, 3, 9] {
            let compressed = compress_chunks_parallel(&data, &chunks, level);

            // Verify each level produces valid compressed data
            let mut reassembled = Vec::with_capacity(data.len());
            for cc in &compressed {
                let decompressed = zstd::decode_all(&cc.compressed_data[..]).unwrap();
                reassembled.extend_from_slice(&decompressed);
            }
            assert_eq!(
                reassembled, data,
                "roundtrip failed at compression level {}",
                level
            );
        }
    }

    // ── format_size helper ─────────────────────────────────────────

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(100), "100 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(64 * 1024), "64.0 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(7 * 1024 * 1024), "7.0 MB");
    }

    // ── Helper ─────────────────────────────────────────────────────

    /// Verify that chunks form a contiguous, non-overlapping cover of the data.
    fn verify_chunks_cover_data(data: &[u8], chunks: &[ContentChunk]) {
        assert!(!chunks.is_empty(), "expected at least one chunk");

        // First chunk starts at 0
        assert_eq!(chunks[0].offset, 0, "first chunk should start at offset 0");

        // Chunks are contiguous
        for i in 1..chunks.len() {
            assert_eq!(
                chunks[i].offset,
                chunks[i - 1].offset + chunks[i - 1].length,
                "chunks {} and {} are not contiguous",
                i - 1,
                i,
            );
        }

        // Total length matches data
        let total: usize = chunks.iter().map(|c| c.length).sum();
        assert_eq!(total, data.len(), "chunks don't cover all data");

        // Last chunk ends at data.len()
        let last = chunks.last().unwrap();
        assert_eq!(
            last.end(),
            data.len(),
            "last chunk doesn't reach end of data"
        );

        // No chunk is empty (except possibly if data is empty, but we checked)
        for chunk in chunks {
            assert!(chunk.length > 0, "chunk {} has zero length", chunk.index);
        }
    }
}

//! Delta transfer negotiation and streaming options.
//!
//! Contains [`ChunkNegotiation`] for computing what chunks need to be
//! transferred, and [`StreamingPushOptions`] / [`StreamingPullOptions`]
//! for configuring streaming transfers.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

use super::layers::{ChunkManifest, LayerSelection};
use super::types::{format_size, ChunkManifestEntry};

// ═══════════════════════════════════════════════════════════════════════
// ChunkNegotiation — delta transfer negotiation result
// ═══════════════════════════════════════════════════════════════════════

/// Result of delta transfer negotiation.
///
/// Given a [`ChunkManifest`] (what the change contains) and a "have" list
/// (chunk hashes the receiver already possesses), this struct describes
/// what still needs to be transferred.
///
/// # Negotiation Flow
///
/// ```text
/// Push:
///   Client:  "My change has chunks [h1, h2, h3, h4, h5]"  (manifest)
///   Server:  "I already have [h1, h3]"                      (have list)
///   Result:  Transfer only [h2, h4, h5]                     (negotiation.needed)
///   Savings: 40% bandwidth saved                            (negotiation.bytes_saved)
///
/// Pull:
///   Server:  "Change X has chunks [h1, h2, h3, h4, h5]"   (manifest)
///   Client:  "I already have [h1, h2, h3]"                 (have list)
///   Result:  Download only [h4, h5]                         (negotiation.needed)
///   Savings: 60% bandwidth saved
/// ```
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::{ChunkManifest, ChunkManifestEntry, ChunkNegotiation};
///
/// let manifest = ChunkManifest::new(vec![
///     ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
///     ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
///     ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
/// ]);
///
/// // Receiver already has chunk 0
/// let have = vec![[0xAA; 32]];
/// let negotiation = ChunkNegotiation::compute(&manifest, &have);
///
/// assert_eq!(negotiation.needed.len(), 2);     // chunks 1 and 2
/// assert_eq!(negotiation.already_have, 1);      // chunk 0
/// assert_eq!(negotiation.bytes_saved, 32000);   // chunk 0's compressed size
/// assert_eq!(negotiation.bytes_needed, 43000);  // chunks 1+2 compressed
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkNegotiation {
    /// Chunk entries that the receiver still needs.
    ///
    /// These must be transferred. Ordered by chunk index.
    pub needed: Vec<ChunkManifestEntry>,

    /// Number of chunks the receiver already has.
    pub already_have: usize,

    /// Compressed bytes saved by not transferring known chunks.
    pub bytes_saved: u64,

    /// Compressed bytes that still need to be transferred.
    pub bytes_needed: u64,

    /// Total chunks in the original manifest.
    pub total_chunks: usize,
}

impl ChunkNegotiation {
    /// Compute the negotiation result from a manifest and a "have" list.
    ///
    /// The "have" list is a set of chunk hashes that the receiver already
    /// possesses. Any chunk in the manifest whose hash matches a "have"
    /// entry is skipped — only the remaining chunks need to be transferred.
    ///
    /// # Arguments
    ///
    /// * `manifest` - The full chunk manifest of the change.
    /// * `have_hashes` - Chunk hashes the receiver already has.
    ///
    /// # Returns
    ///
    /// A `ChunkNegotiation` describing what still needs to be transferred.
    pub fn compute(manifest: &ChunkManifest, have_hashes: &[[u8; 32]]) -> Self {
        let have_set: HashSet<[u8; 32]> = have_hashes.iter().copied().collect();

        let mut needed = Vec::new();
        let mut already_have = 0usize;
        let mut bytes_saved = 0u64;
        let mut bytes_needed = 0u64;

        for entry in &manifest.entries {
            if have_set.contains(&entry.hash) {
                already_have += 1;
                bytes_saved += entry.compressed_size as u64;
            } else {
                bytes_needed += entry.compressed_size as u64;
                needed.push(*entry);
            }
        }

        Self {
            needed,
            already_have,
            bytes_saved,
            bytes_needed,
            total_chunks: manifest.chunk_count(),
        }
    }

    /// Returns `true` if all chunks are already present (nothing to transfer).
    pub fn is_complete(&self) -> bool {
        self.needed.is_empty()
    }

    /// Returns `true` if no chunks are present (full transfer needed).
    pub fn is_full_transfer(&self) -> bool {
        self.already_have == 0
    }

    /// Returns the percentage of bytes saved (0.0 to 100.0).
    pub fn savings_pct(&self) -> f64 {
        let total = self.bytes_saved + self.bytes_needed;
        if total == 0 {
            return 0.0;
        }
        self.bytes_saved as f64 / total as f64 * 100.0
    }

    /// Returns the set of needed chunk hashes.
    pub fn needed_hashes(&self) -> HashSet<[u8; 32]> {
        self.needed.iter().map(|e| e.hash).collect()
    }

    /// Returns the set of needed chunk indices.
    pub fn needed_indices(&self) -> Vec<u32> {
        self.needed.iter().map(|e| e.index).collect()
    }
}

impl fmt::Display for ChunkNegotiation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "need {} of {} chunks ({} to transfer, {} saved, {:.1}% savings)",
            self.needed.len(),
            self.total_chunks,
            format_size(self.bytes_needed),
            format_size(self.bytes_saved),
            self.savings_pct(),
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StreamingPushOptions — configuration for streaming uploads
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for streaming push operations.
///
/// Controls whether to use delta transfer (chunk manifest negotiation),
/// which layers to upload, and progress reporting behavior.
///
/// # Default Behavior
///
/// By default, a push uploads all layers and uses delta transfer if the
/// server supports it. The `simple()` preset disables delta transfer for
/// maximum compatibility.
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::StreamingPushOptions;
///
/// // Default: all layers, delta transfer enabled
/// let opts = StreamingPushOptions::default();
/// assert!(opts.use_delta_transfer);
///
/// // Simple mode: no delta transfer
/// let opts = StreamingPushOptions::simple();
/// assert!(!opts.use_delta_transfer);
/// ```
#[derive(Clone, Debug)]
pub struct StreamingPushOptions {
    /// Whether to negotiate delta transfer using chunk manifests.
    ///
    /// When enabled, the client sends its chunk hashes to the server
    /// first, then uploads only the chunks the server doesn't have.
    /// When disabled, the full change file is uploaded as-is.
    ///
    /// Default: `true`.
    pub use_delta_transfer: bool,

    /// Whether to report per-section progress.
    ///
    /// Default: `true`.
    pub report_progress: bool,

    /// Maximum number of chunks to upload in parallel.
    ///
    /// Only relevant when `use_delta_transfer` is enabled.
    /// Default: `4`.
    pub max_parallel_chunks: usize,
}

impl StreamingPushOptions {
    /// Simple push: upload full change, no delta transfer.
    pub fn simple() -> Self {
        Self {
            use_delta_transfer: false,
            report_progress: true,
            max_parallel_chunks: 1,
        }
    }
}

impl Default for StreamingPushOptions {
    fn default() -> Self {
        Self {
            use_delta_transfer: true,
            report_progress: true,
            max_parallel_chunks: 4,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StreamingPullOptions — configuration for streaming downloads
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for streaming pull operations.
///
/// Controls which layers to download, whether to use delta transfer for
/// content chunks, and progress reporting behavior.
///
/// # Default Behavior
///
/// By default, a pull downloads all layers. Use [`thin_pull()`](Self::thin_pull)
/// to skip the semantic layer, or [`thin_review()`](Self::thin_review) to skip
/// the graph layer.
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::{StreamingPullOptions, Layer};
///
/// // Full pull (default)
/// let opts = StreamingPullOptions::default();
/// assert!(opts.layers.includes(Layer::Graph));
/// assert!(opts.layers.includes(Layer::Semantic));
///
/// // Thin pull (graph + content only)
/// let opts = StreamingPullOptions::thin_pull();
/// assert!(opts.layers.includes(Layer::Graph));
/// assert!(!opts.layers.includes(Layer::Semantic));
///
/// // With delta transfer
/// let opts = StreamingPullOptions::default()
///     .with_delta_transfer(true);
/// assert!(opts.use_delta_transfer);
/// ```
#[derive(Clone, Debug)]
pub struct StreamingPullOptions {
    /// Which layers to download.
    ///
    /// Default: all layers.
    pub layers: LayerSelection,

    /// Whether to negotiate delta transfer for content chunks.
    ///
    /// When enabled, the client sends its known chunk hashes before
    /// downloading, and the server only streams chunks the client
    /// doesn't have. Requires a second round trip but can save
    /// significant bandwidth for incremental pulls.
    ///
    /// Default: `false` (simpler, single round trip).
    pub use_delta_transfer: bool,

    /// Whether to report per-section progress.
    ///
    /// Default: `true`.
    pub report_progress: bool,

    /// Whether to verify the content hash after download.
    ///
    /// Default: `true`.
    pub verify_hash: bool,
}

impl StreamingPullOptions {
    /// Thin pull: graph + content only, skip semantic.
    pub fn thin_pull() -> Self {
        Self {
            layers: LayerSelection::thin_pull(),
            use_delta_transfer: false,
            report_progress: true,
            verify_hash: true,
        }
    }

    /// Thin review: semantic + content only, skip graph.
    pub fn thin_review() -> Self {
        Self {
            layers: LayerSelection::thin_review(),
            use_delta_transfer: false,
            report_progress: true,
            verify_hash: true,
        }
    }

    /// Enable or disable delta transfer.
    pub fn with_delta_transfer(mut self, enabled: bool) -> Self {
        self.use_delta_transfer = enabled;
        self
    }

    /// Enable or disable hash verification.
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify_hash = verify;
        self
    }
}

impl Default for StreamingPullOptions {
    fn default() -> Self {
        Self {
            layers: LayerSelection::all(),
            use_delta_transfer: false,
            report_progress: true,
            verify_hash: true,
        }
    }
}


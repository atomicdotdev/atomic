//! Streaming push/pull protocol for V3 change files.
//!
//! This module defines the protocol types and helpers for streaming V3 change
//! files over HTTP without full-body buffering. The V3 section-based format
//! is inherently streaming — each section is independently compressed and
//! can be processed as it arrives.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                        Streaming Push                                    │
//! │                                                                          │
//! │  Client                          Server                                  │
//! │  ┌────────────────────┐         ┌────────────────────┐                  │
//! │  │ .change file       │         │ ChangeReader        │                  │
//! │  │ (V3 on disk)       │         │ (section-by-section)│                  │
//! │  └──────┬─────────────┘         └──────▲─────────────┘                  │
//! │         │ read sections                │ process sections                │
//! │         ▼                              │                                 │
//! │  ┌────────────────────┐  HTTP   ┌─────┴──────────────┐                  │
//! │  │ StreamingUpload    │ ──────▶ │ HTTP body stream   │                  │
//! │  │ (chunked transfer) │         │ (apply-as-you-go)  │                  │
//! │  └────────────────────┘         └────────────────────┘                  │
//! │                                                                          │
//! │  Memory: O(section_size)        Memory: O(section_size)                 │
//! │  NOT O(total_change_size)       NOT O(total_change_size)                │
//! └──────────────────────────────────────────────────────────────────────────┘
//!
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                        Delta Push (with chunk manifest)                  │
//! │                                                                          │
//! │  Client                          Server                                  │
//! │                                                                          │
//! │  1. POST manifest ──────────▶  "I have chunks [h1, h2, h3, h4, h5]"    │
//! │                                                                          │
//! │  2. ◀────────── need list ──  "Send me [h3, h5] — I have the rest"     │
//! │                                                                          │
//! │  3. POST sections ──────────▶  Stream only needed metadata + chunks     │
//! │                                 Server fills in known chunks from        │
//! │                                 its CONTENT_CHUNKS table                 │
//! │                                                                          │
//! │  Savings: For a 1-line edit to a 10 MB file, transfer ~64 KB            │
//! │           instead of 10 MB (only the changed chunk).                     │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Protocol Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`LayerSelection`] | Which layers to include in a download |
//! | [`ChunkManifest`] | Ordered list of content chunks in a change |
//! | [`ChunkManifestEntry`] | One entry: (index, hash, compressed_size) |
//! | [`ChunkNegotiation`] | Client's "have" list → Server's "need" response |
//! | [`StreamingPushOptions`] | Configuration for streaming uploads |
//! | [`StreamingPullOptions`] | Configuration for streaming downloads |
//! | [`TransferProgress`] | Per-section progress reporting |
//! | [`TransferStats`] | Summary statistics for a completed transfer |
//!
//! # Wire Protocol Extensions
//!
//! These types extend the existing HTTP protocol with query parameters:
//!
//! | Parameter | Example | Meaning |
//! |-----------|---------|---------|
//! | `layers` | `?change={h}&layers=graph,content` | Thin pull — skip SEMANTIC |
//! | `manifest` | `?change={h}&manifest` | Get chunk manifest only |
//! | `have` | POST body with chunk hashes | Delta push — skip known chunks |
//!
//! # Layer-Selective Pull
//!
//! The `layers` parameter controls which sections the server includes:
//!
//! | Value | Sections Included | Use Case |
//! |-------|-------------------|----------|
//! | `all` (default) | Everything | Full clone/pull |
//! | `graph,content` | HEADER+DEPS+GRAPH+CONTENT+trailer | Thin pull (apply only) |
//! | `semantic,content` | HEADER+SEMANTIC+CONTENT+trailer | Thin review (display only) |
//! | `graph` | HEADER+DEPS+GRAPH+trailer | Ultra-thin (no content) |
//! | `manifest` | Chunk manifest JSON | Pre-fetch chunk inventory |
//!
//! Metadata sections (HEADER, DEPS, PROVENANCE) are always included when
//! any layer is requested — they're tiny and needed for context.
//!
//! # Examples
//!
//! ## Layer-Selective Download
//!
//! ```rust
//! use atomic_remote::streaming::{LayerSelection, Layer};
//!
//! // Full download (default)
//! let sel = LayerSelection::all();
//! assert_eq!(sel.to_query_value(), "all");
//!
//! // Thin pull (graph + content only, skip semantic)
//! let sel = LayerSelection::thin_pull();
//! assert_eq!(sel.to_query_value(), "graph,content");
//! assert!(sel.includes(Layer::Graph));
//! assert!(sel.includes(Layer::Content));
//! assert!(!sel.includes(Layer::Semantic));
//!
//! // Thin review (semantic + content, skip graph)
//! let sel = LayerSelection::thin_review();
//! assert_eq!(sel.to_query_value(), "semantic,content");
//! ```
//!
//! ## Chunk Manifest
//!
//! ```rust
//! use atomic_remote::streaming::{ChunkManifest, ChunkManifestEntry};
//!
//! let manifest = ChunkManifest::new(vec![
//!     ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
//!     ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
//!     ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
//! ]);
//!
//! assert_eq!(manifest.chunk_count(), 3);
//! assert_eq!(manifest.total_compressed(), 75000);
//!
//! // Serialize for the wire
//! let json = serde_json::to_string(&manifest).unwrap();
//! let decoded: ChunkManifest = serde_json::from_str(&json).unwrap();
//! assert_eq!(decoded.chunk_count(), 3);
//! ```
//!
//! ## Delta Negotiation
//!
//! ```rust
//! use atomic_remote::streaming::{ChunkManifest, ChunkManifestEntry, ChunkNegotiation};
//!
//! // Server has a manifest for the change
//! let manifest = ChunkManifest::new(vec![
//!     ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
//!     ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
//!     ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
//! ]);
//!
//! // Client already has chunks 0 and 1
//! let have: Vec<[u8; 32]> = vec![[0xAA; 32], [0xBB; 32]];
//!
//! // Compute what the client still needs
//! let negotiation = ChunkNegotiation::compute(&manifest, &have);
//! assert_eq!(negotiation.needed.len(), 1); // only chunk 2
//! assert_eq!(negotiation.already_have, 2);
//! assert_eq!(negotiation.bytes_saved, 60000); // skipped 32000 + 28000
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// Layer — individual protocol layers
// ═══════════════════════════════════════════════════════════════════════

/// An individual layer in the V3 change file.
///
/// Layers correspond to groups of section types:
/// - `Graph` = GRAPH sections (storage/merge)
/// - `Semantic` = SEMANTIC sections (display/review)
/// - `Content` = CONTENT chunks (raw file data)
///
/// Metadata sections (HEADER, DEPS, PROVENANCE) are always included
/// and don't need to be selected — they're needed for any operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// Graph operations (GRAPH sections) — storage/merge layer.
    ///
    /// Required to apply a change to the repository DAG.
    Graph,

    /// Semantic operations (SEMANTIC sections) — display/review layer.
    ///
    /// Required for line-level diffs, token-level blame, and code review.
    /// Can be regenerated from graph + content if missing.
    Semantic,

    /// Content chunks (CONTENT sections) — raw file data.
    ///
    /// Required to output file contents. Can be delta-transferred
    /// (only send chunks the receiver doesn't already have).
    Content,
}

impl Layer {
    /// Parse a layer from a string.
    ///
    /// Accepts: "graph", "semantic", "content" (case-insensitive).
    ///
    /// Returns `None` for unrecognized strings.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "graph" => Some(Layer::Graph),
            "semantic" => Some(Layer::Semantic),
            "content" => Some(Layer::Content),
            _ => None,
        }
    }

    /// Returns the string representation for use in query parameters.
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Graph => "graph",
            Layer::Semantic => "semantic",
            Layer::Content => "content",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// LayerSelection — which layers to include in a transfer
// ═══════════════════════════════════════════════════════════════════════

/// Specifies which layers to include in a change download.
///
/// This maps to the `?layers=` query parameter in the HTTP protocol.
/// Metadata sections (HEADER, DEPS, PROVENANCE) are always included
/// regardless of the selection — they're small and universally needed.
///
/// # Common Presets
///
/// | Preset | Layers | Query Value | Use Case |
/// |--------|--------|-------------|----------|
/// | [`all()`](Self::all) | Graph+Semantic+Content | `all` | Full clone/pull |
/// | [`thin_pull()`](Self::thin_pull) | Graph+Content | `graph,content` | Apply-only pull |
/// | [`thin_review()`](Self::thin_review) | Semantic+Content | `semantic,content` | Code review |
/// | [`graph_only()`](Self::graph_only) | Graph | `graph` | Ultra-thin metadata |
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::{LayerSelection, Layer};
///
/// let sel = LayerSelection::thin_pull();
/// assert!(sel.includes(Layer::Graph));
/// assert!(sel.includes(Layer::Content));
/// assert!(!sel.includes(Layer::Semantic));
/// assert_eq!(sel.to_query_value(), "graph,content");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerSelection {
    /// The set of selected layers.
    layers: HashSet<Layer>,
}

impl LayerSelection {
    /// Select all layers (full download).
    ///
    /// This is the default — equivalent to not specifying `?layers=` at all.
    pub fn all() -> Self {
        let mut layers = HashSet::new();
        layers.insert(Layer::Graph);
        layers.insert(Layer::Semantic);
        layers.insert(Layer::Content);
        Self { layers }
    }

    /// Thin pull: graph + content only.
    ///
    /// Downloads the minimum needed to **apply** a change to the local
    /// repository. Semantic sections are skipped — they can be regenerated
    /// locally from graph + content.
    ///
    /// Typical savings: ~40% smaller download (semantic is ~40% of a change).
    pub fn thin_pull() -> Self {
        let mut layers = HashSet::new();
        layers.insert(Layer::Graph);
        layers.insert(Layer::Content);
        Self { layers }
    }

    /// Thin review: semantic + content only.
    ///
    /// Downloads the minimum needed to **display** diffs, blame, and code
    /// review. Graph sections are skipped — they're only needed for applying
    /// the change to the DAG.
    ///
    /// Typical savings: ~60% smaller download (graph is ~60% of a change).
    pub fn thin_review() -> Self {
        let mut layers = HashSet::new();
        layers.insert(Layer::Semantic);
        layers.insert(Layer::Content);
        Self { layers }
    }

    /// Graph only: no content or semantic.
    ///
    /// Downloads only the graph operations. Useful for dependency analysis
    /// and metadata inspection without downloading file content.
    pub fn graph_only() -> Self {
        let mut layers = HashSet::new();
        layers.insert(Layer::Graph);
        Self { layers }
    }

    /// Create a custom selection from a set of layers.
    pub fn custom(layers: impl IntoIterator<Item = Layer>) -> Self {
        Self {
            layers: layers.into_iter().collect(),
        }
    }

    /// Parse from a query parameter value.
    ///
    /// Accepts:
    /// - `"all"` → all layers
    /// - `"graph,content"` → graph + content
    /// - `"semantic,content"` → semantic + content
    /// - `"graph"` → graph only
    /// - Any comma-separated combination of layer names
    ///
    /// Unknown layer names are silently ignored.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_remote::streaming::{LayerSelection, Layer};
    ///
    /// let sel = LayerSelection::from_query_value("graph,content");
    /// assert!(sel.includes(Layer::Graph));
    /// assert!(sel.includes(Layer::Content));
    /// assert!(!sel.includes(Layer::Semantic));
    ///
    /// let sel = LayerSelection::from_query_value("all");
    /// assert!(sel.includes(Layer::Graph));
    /// assert!(sel.includes(Layer::Semantic));
    /// assert!(sel.includes(Layer::Content));
    /// ```
    pub fn from_query_value(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            return Self::all();
        }

        let layers: HashSet<Layer> = trimmed
            .split(',')
            .filter_map(|s| Layer::from_str_loose(s.trim()))
            .collect();

        Self { layers }
    }

    /// Serialize to a query parameter value.
    ///
    /// Returns `"all"` if all three layers are selected, otherwise
    /// returns a comma-separated list of layer names in canonical order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_remote::streaming::LayerSelection;
    ///
    /// assert_eq!(LayerSelection::all().to_query_value(), "all");
    /// assert_eq!(LayerSelection::thin_pull().to_query_value(), "graph,content");
    /// assert_eq!(LayerSelection::thin_review().to_query_value(), "semantic,content");
    /// assert_eq!(LayerSelection::graph_only().to_query_value(), "graph");
    /// ```
    pub fn to_query_value(&self) -> String {
        if self.is_all() {
            return "all".to_string();
        }

        // Canonical order: graph, semantic, content
        let mut parts = Vec::new();
        if self.layers.contains(&Layer::Graph) {
            parts.push("graph");
        }
        if self.layers.contains(&Layer::Semantic) {
            parts.push("semantic");
        }
        if self.layers.contains(&Layer::Content) {
            parts.push("content");
        }

        parts.join(",")
    }

    /// Returns `true` if the given layer is included in this selection.
    #[inline]
    pub fn includes(&self, layer: Layer) -> bool {
        self.layers.contains(&layer)
    }

    /// Returns `true` if all three layers are selected (full download).
    pub fn is_all(&self) -> bool {
        self.layers.contains(&Layer::Graph)
            && self.layers.contains(&Layer::Semantic)
            && self.layers.contains(&Layer::Content)
    }

    /// Returns `true` if no layers are selected.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Returns the number of selected layers.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Returns an iterator over the selected layers.
    pub fn iter(&self) -> impl Iterator<Item = &Layer> {
        self.layers.iter()
    }
}

impl Default for LayerSelection {
    /// Default is all layers (full download).
    fn default() -> Self {
        Self::all()
    }
}

impl fmt::Display for LayerSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "layers={}", self.to_query_value())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ChunkManifestEntry — one entry in the chunk manifest
// ═══════════════════════════════════════════════════════════════════════

/// A single entry in a [`ChunkManifest`].
///
/// Describes one content chunk: its sequential index, content-address hash,
/// compressed size, and uncompressed size. This is enough information for
/// a receiver to determine whether it already has the chunk (by hash) and
/// how much data it would need to download (compressed size).
///
/// # Content Addressing
///
/// The `hash` is the blake3 hash of the **uncompressed** chunk data. Two
/// chunks with identical content produce identical hashes regardless of
/// which change or file they came from. This enables cross-change dedup.
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::ChunkManifestEntry;
///
/// let entry = ChunkManifestEntry::new(0, [0xAA; 32], 30000, 65536);
/// assert_eq!(entry.index, 0);
/// assert_eq!(entry.compressed_size, 30000);
/// assert_eq!(entry.uncompressed_size, 65536);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkManifestEntry {
    /// Sequential chunk index (0-based) within the change.
    pub index: u32,

    /// Blake3 hash of the uncompressed chunk data.
    ///
    /// This is the content address — identical content produces identical
    /// hashes. Used by the receiver to check "do I already have this?"
    #[serde(with = "hex_hash")]
    pub hash: [u8; 32],

    /// Compressed size in bytes (what gets transferred over the wire).
    pub compressed_size: u32,

    /// Uncompressed size in bytes (what the chunk decompresses to).
    pub uncompressed_size: u32,
}

impl ChunkManifestEntry {
    /// Create a new manifest entry.
    pub const fn new(
        index: u32,
        hash: [u8; 32],
        compressed_size: u32,
        uncompressed_size: u32,
    ) -> Self {
        Self {
            index,
            hash,
            compressed_size,
            uncompressed_size,
        }
    }

    /// Returns the compression ratio (compressed / uncompressed).
    pub fn compression_ratio(&self) -> f64 {
        if self.uncompressed_size == 0 {
            return f64::NAN;
        }
        self.compressed_size as f64 / self.uncompressed_size as f64
    }
}

impl fmt::Display for ChunkManifestEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "chunk#{} {:02x}{:02x}{:02x}{:02x}… {} → {} bytes",
            self.index,
            self.hash[0],
            self.hash[1],
            self.hash[2],
            self.hash[3],
            self.uncompressed_size,
            self.compressed_size,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ChunkManifest — ordered list of content chunks in a change
// ═══════════════════════════════════════════════════════════════════════

/// An ordered manifest of all content chunks in a change.
///
/// The manifest is the starting point for delta negotiation: the client
/// sends its "have" list (chunk hashes it already possesses), and the
/// server responds with only the chunks the client doesn't have.
///
/// # Wire Format
///
/// The manifest is serialized as JSON for the `?manifest` endpoint:
///
/// ```json
/// {
///   "entries": [
///     {"index": 0, "hash": "aabb...", "compressed_size": 32000, "uncompressed_size": 65536},
///     {"index": 1, "hash": "ccdd...", "compressed_size": 28000, "uncompressed_size": 65536}
///   ]
/// }
/// ```
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::{ChunkManifest, ChunkManifestEntry};
///
/// let manifest = ChunkManifest::new(vec![
///     ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
///     ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
/// ]);
///
/// assert_eq!(manifest.chunk_count(), 2);
/// assert_eq!(manifest.total_compressed(), 60000);
/// assert_eq!(manifest.total_uncompressed(), 131072);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkManifest {
    /// Ordered list of chunk entries.
    pub entries: Vec<ChunkManifestEntry>,
}

impl ChunkManifest {
    /// Create a new manifest from a list of entries.
    pub fn new(entries: Vec<ChunkManifestEntry>) -> Self {
        Self { entries }
    }

    /// Create an empty manifest (no content chunks).
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Returns the number of chunks.
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the manifest has no chunks.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total compressed size across all chunks (bytes on the wire).
    pub fn total_compressed(&self) -> u64 {
        self.entries.iter().map(|e| e.compressed_size as u64).sum()
    }

    /// Total uncompressed size across all chunks (original data size).
    pub fn total_uncompressed(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.uncompressed_size as u64)
            .sum()
    }

    /// Returns a set of all chunk hashes in this manifest.
    ///
    /// Used for quick "do I have this?" lookups during negotiation.
    pub fn hash_set(&self) -> HashSet<[u8; 32]> {
        self.entries.iter().map(|e| e.hash).collect()
    }

    /// Find a chunk entry by its hash.
    ///
    /// Returns `None` if no chunk with the given hash exists.
    pub fn find_by_hash(&self, hash: &[u8; 32]) -> Option<&ChunkManifestEntry> {
        self.entries.iter().find(|e| &e.hash == hash)
    }

    /// Find a chunk entry by its index.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn find_by_index(&self, index: u32) -> Option<&ChunkManifestEntry> {
        self.entries.iter().find(|e| e.index == index)
    }

    /// Returns an iterator over all chunk hashes in order.
    pub fn hashes(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.entries.iter().map(|e| &e.hash)
    }
}

impl fmt::Display for ChunkManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ChunkManifest({} chunks, {} compressed, {} uncompressed)",
            self.chunk_count(),
            format_size(self.total_compressed()),
            format_size(self.total_uncompressed()),
        )
    }
}

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

// ═══════════════════════════════════════════════════════════════════════
// TransferProgress — per-section progress reporting
// ═══════════════════════════════════════════════════════════════════════

/// Progress event emitted during a streaming transfer.
///
/// These events are emitted as sections are transferred, enabling
/// real-time progress reporting in the CLI.
///
/// # Event Flow (Push)
///
/// ```text
/// Started { total_sections: 11, total_bytes: 7500000 }
/// SectionComplete { section: "HEADER", bytes: 200 }
/// SectionComplete { section: "DEPS", bytes: 50 }
/// SectionComplete { section: "GRAPH #1", bytes: 15000 }
/// ChunkComplete { index: 0, bytes: 32000, hash: "aabb..." }
/// ChunkComplete { index: 1, bytes: 28000, hash: "ccdd..." }
/// ...
/// Finished { total_bytes: 7500000, elapsed_ms: 1200 }
/// ```
#[derive(Clone, Debug)]
pub enum TransferProgress {
    /// Transfer is starting.
    Started {
        /// Total number of sections to transfer.
        total_sections: u32,
        /// Estimated total bytes to transfer (compressed).
        total_bytes_estimate: u64,
    },

    /// A metadata or layer section was transferred.
    SectionComplete {
        /// Human-readable section description (e.g., "HEADER", "GRAPH #3").
        section: String,
        /// Compressed bytes transferred for this section.
        bytes_transferred: u64,
    },

    /// A content chunk was transferred.
    ChunkComplete {
        /// Chunk index.
        index: u32,
        /// Compressed bytes transferred.
        bytes_transferred: u32,
        /// Was this chunk skipped (already present on receiver)?
        skipped: bool,
    },

    /// Transfer is complete.
    Finished {
        /// Total compressed bytes transferred.
        total_bytes: u64,
        /// Wall-clock elapsed time in milliseconds.
        elapsed_ms: u64,
    },
}

impl fmt::Display for TransferProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransferProgress::Started {
                total_sections,
                total_bytes_estimate,
            } => write!(
                f,
                "Starting transfer: {} sections, ~{}",
                total_sections,
                format_size(*total_bytes_estimate),
            ),
            TransferProgress::SectionComplete {
                section,
                bytes_transferred,
            } => write!(f, "  {} {}", section, format_size(*bytes_transferred),),
            TransferProgress::ChunkComplete {
                index,
                bytes_transferred,
                skipped,
            } => {
                if *skipped {
                    write!(f, "  chunk #{} (skipped — already present)", index)
                } else {
                    write!(
                        f,
                        "  chunk #{} {}",
                        index,
                        format_size(*bytes_transferred as u64),
                    )
                }
            }
            TransferProgress::Finished {
                total_bytes,
                elapsed_ms,
            } => write!(
                f,
                "Transfer complete: {} in {:.1}s",
                format_size(*total_bytes),
                *elapsed_ms as f64 / 1000.0,
            ),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// TransferStats — summary statistics for a completed transfer
// ═══════════════════════════════════════════════════════════════════════

/// Summary statistics for a completed streaming transfer.
///
/// Available after a push or pull operation completes. Useful for
/// logging, performance analysis, and displaying summaries to the user.
///
/// # Examples
///
/// ```rust
/// use atomic_remote::streaming::TransferStats;
///
/// let stats = TransferStats {
///     sections_transferred: 11,
///     chunks_transferred: 5,
///     chunks_skipped: 3,
///     bytes_transferred: 75000,
///     bytes_skipped: 60000,
///     elapsed_ms: 1200,
/// };
///
/// assert_eq!(stats.total_chunks(), 8);
/// assert!((stats.savings_pct() - 44.4).abs() < 1.0);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// Number of sections transferred (including metadata, graph, semantic).
    pub sections_transferred: u32,

    /// Number of content chunks actually transferred (not skipped).
    pub chunks_transferred: u32,

    /// Number of content chunks skipped (already present on receiver).
    pub chunks_skipped: u32,

    /// Total compressed bytes actually transferred.
    pub bytes_transferred: u64,

    /// Total compressed bytes saved by delta transfer (skipped chunks).
    pub bytes_skipped: u64,

    /// Wall-clock elapsed time in milliseconds.
    pub elapsed_ms: u64,
}

impl TransferStats {
    /// Total chunks (transferred + skipped).
    pub fn total_chunks(&self) -> u32 {
        self.chunks_transferred + self.chunks_skipped
    }

    /// Total bytes (transferred + skipped).
    pub fn total_bytes(&self) -> u64 {
        self.bytes_transferred + self.bytes_skipped
    }

    /// Percentage of bytes saved by delta transfer (0.0 to 100.0).
    pub fn savings_pct(&self) -> f64 {
        let total = self.total_bytes();
        if total == 0 {
            return 0.0;
        }
        self.bytes_skipped as f64 / total as f64 * 100.0
    }

    /// Effective transfer rate in bytes per second.
    ///
    /// Returns 0 if elapsed_ms is 0.
    pub fn bytes_per_second(&self) -> u64 {
        if self.elapsed_ms == 0 {
            return 0;
        }
        self.bytes_transferred * 1000 / self.elapsed_ms
    }
}

impl fmt::Display for TransferStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} sections, {} chunks transferred",
            self.sections_transferred, self.chunks_transferred,
        )?;

        if self.chunks_skipped > 0 {
            write!(
                f,
                " ({} skipped, {:.1}% savings)",
                self.chunks_skipped,
                self.savings_pct(),
            )?;
        }

        write!(
            f,
            ", {} in {:.1}s",
            format_size(self.bytes_transferred),
            self.elapsed_ms as f64 / 1000.0,
        )?;

        let bps = self.bytes_per_second();
        if bps > 0 {
            write!(f, " ({}/s)", format_size(bps))?;
        }

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helper: hex serialization for [u8; 32]
// ═══════════════════════════════════════════════════════════════════════

/// Serde module for hex-encoding [u8; 32] in JSON.
///
/// Serializes as a 64-character lowercase hex string, deserializes from
/// the same. This makes chunk manifests human-readable in JSON.
mod hex_hash {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(hash: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s.len() != 64 {
            return Err(serde::de::Error::custom(format!(
                "expected 64 hex chars, got {}",
                s.len()
            )));
        }

        let mut hash = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex_str =
                std::str::from_utf8(chunk).map_err(|_| serde::de::Error::custom("invalid utf8"))?;
            hash[i] = u8::from_str_radix(hex_str, 16)
                .map_err(|_| serde::de::Error::custom(format!("invalid hex: {}", hex_str)))?;
        }

        Ok(hash)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helper: format byte sizes
// ═══════════════════════════════════════════════════════════════════════

/// Format a byte count as a human-readable string.
fn format_size(bytes: u64) -> String {
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

    // ── Layer ───────────────────────────────────────────────────────

    #[test]
    fn test_layer_from_str() {
        assert_eq!(Layer::from_str_loose("graph"), Some(Layer::Graph));
        assert_eq!(Layer::from_str_loose("GRAPH"), Some(Layer::Graph));
        assert_eq!(Layer::from_str_loose("semantic"), Some(Layer::Semantic));
        assert_eq!(Layer::from_str_loose("content"), Some(Layer::Content));
        assert_eq!(Layer::from_str_loose("unknown"), None);
        assert_eq!(Layer::from_str_loose(""), None);
    }

    #[test]
    fn test_layer_as_str() {
        assert_eq!(Layer::Graph.as_str(), "graph");
        assert_eq!(Layer::Semantic.as_str(), "semantic");
        assert_eq!(Layer::Content.as_str(), "content");
    }

    #[test]
    fn test_layer_display() {
        assert_eq!(format!("{}", Layer::Graph), "graph");
        assert_eq!(format!("{}", Layer::Semantic), "semantic");
        assert_eq!(format!("{}", Layer::Content), "content");
    }

    #[test]
    fn test_layer_json_roundtrip() {
        for layer in [Layer::Graph, Layer::Semantic, Layer::Content] {
            let json = serde_json::to_string(&layer).unwrap();
            let decoded: Layer = serde_json::from_str(&json).unwrap();
            assert_eq!(layer, decoded);
        }
    }

    // ── LayerSelection ─────────────────────────────────────────────

    #[test]
    fn test_layer_selection_all() {
        let sel = LayerSelection::all();
        assert!(sel.includes(Layer::Graph));
        assert!(sel.includes(Layer::Semantic));
        assert!(sel.includes(Layer::Content));
        assert!(sel.is_all());
        assert!(!sel.is_empty());
        assert_eq!(sel.len(), 3);
        assert_eq!(sel.to_query_value(), "all");
    }

    #[test]
    fn test_layer_selection_thin_pull() {
        let sel = LayerSelection::thin_pull();
        assert!(sel.includes(Layer::Graph));
        assert!(!sel.includes(Layer::Semantic));
        assert!(sel.includes(Layer::Content));
        assert!(!sel.is_all());
        assert_eq!(sel.len(), 2);
        assert_eq!(sel.to_query_value(), "graph,content");
    }

    #[test]
    fn test_layer_selection_thin_review() {
        let sel = LayerSelection::thin_review();
        assert!(!sel.includes(Layer::Graph));
        assert!(sel.includes(Layer::Semantic));
        assert!(sel.includes(Layer::Content));
        assert!(!sel.is_all());
        assert_eq!(sel.to_query_value(), "semantic,content");
    }

    #[test]
    fn test_layer_selection_graph_only() {
        let sel = LayerSelection::graph_only();
        assert!(sel.includes(Layer::Graph));
        assert!(!sel.includes(Layer::Semantic));
        assert!(!sel.includes(Layer::Content));
        assert_eq!(sel.to_query_value(), "graph");
    }

    #[test]
    fn test_layer_selection_custom() {
        let sel = LayerSelection::custom([Layer::Semantic]);
        assert!(!sel.includes(Layer::Graph));
        assert!(sel.includes(Layer::Semantic));
        assert!(!sel.includes(Layer::Content));
        assert_eq!(sel.to_query_value(), "semantic");
    }

    #[test]
    fn test_layer_selection_from_query_all() {
        let sel = LayerSelection::from_query_value("all");
        assert!(sel.is_all());

        let sel = LayerSelection::from_query_value("ALL");
        assert!(sel.is_all());
    }

    #[test]
    fn test_layer_selection_from_query_thin_pull() {
        let sel = LayerSelection::from_query_value("graph,content");
        assert!(sel.includes(Layer::Graph));
        assert!(!sel.includes(Layer::Semantic));
        assert!(sel.includes(Layer::Content));
    }

    #[test]
    fn test_layer_selection_from_query_with_spaces() {
        let sel = LayerSelection::from_query_value(" graph , content ");
        assert!(sel.includes(Layer::Graph));
        assert!(sel.includes(Layer::Content));
    }

    #[test]
    fn test_layer_selection_from_query_unknown_ignored() {
        let sel = LayerSelection::from_query_value("graph,unknown,content");
        assert!(sel.includes(Layer::Graph));
        assert!(sel.includes(Layer::Content));
        assert!(!sel.includes(Layer::Semantic));
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn test_layer_selection_from_query_empty() {
        let sel = LayerSelection::from_query_value("");
        assert!(sel.is_empty());
    }

    #[test]
    fn test_layer_selection_default_is_all() {
        let sel = LayerSelection::default();
        assert!(sel.is_all());
    }

    #[test]
    fn test_layer_selection_display() {
        assert_eq!(format!("{}", LayerSelection::all()), "layers=all");
        assert_eq!(
            format!("{}", LayerSelection::thin_pull()),
            "layers=graph,content"
        );
    }

    #[test]
    fn test_layer_selection_query_roundtrip() {
        for sel in [
            LayerSelection::all(),
            LayerSelection::thin_pull(),
            LayerSelection::thin_review(),
            LayerSelection::graph_only(),
        ] {
            let query = sel.to_query_value();
            let decoded = LayerSelection::from_query_value(&query);
            assert_eq!(sel, decoded, "roundtrip failed for '{}'", query);
        }
    }

    #[test]
    fn test_layer_selection_json_roundtrip() {
        let sel = LayerSelection::thin_pull();
        let json = serde_json::to_string(&sel).unwrap();
        let decoded: LayerSelection = serde_json::from_str(&json).unwrap();
        assert_eq!(sel, decoded);
    }

    // ── ChunkManifestEntry ─────────────────────────────────────────

    #[test]
    fn test_manifest_entry_new() {
        let entry = ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536);
        assert_eq!(entry.index, 0);
        assert_eq!(entry.hash, [0xAA; 32]);
        assert_eq!(entry.compressed_size, 32000);
        assert_eq!(entry.uncompressed_size, 65536);
    }

    #[test]
    fn test_manifest_entry_compression_ratio() {
        let entry = ChunkManifestEntry::new(0, [0; 32], 500, 1000);
        assert!((entry.compression_ratio() - 0.5).abs() < f64::EPSILON);

        let zero = ChunkManifestEntry::new(0, [0; 32], 0, 0);
        assert!(zero.compression_ratio().is_nan());
    }

    #[test]
    fn test_manifest_entry_display() {
        let entry = ChunkManifestEntry::new(
            3,
            [
                0xAB, 0xCD, 0xEF, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0,
            ],
            30000,
            65536,
        );
        let display = format!("{}", entry);
        assert!(display.contains("chunk#3"));
        assert!(display.contains("abcdef01"));
    }

    #[test]
    fn test_manifest_entry_json_roundtrip() {
        let entry = ChunkManifestEntry::new(5, [0x42; 32], 12345, 67890);
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: ChunkManifestEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn test_manifest_entry_json_format() {
        let entry = ChunkManifestEntry::new(0, [0xAB; 32], 100, 200);
        let json = serde_json::to_string_pretty(&entry).unwrap();
        // Hash should be hex-encoded
        assert!(json.contains("abababab"));
        assert!(json.contains("\"index\": 0"));
        assert!(json.contains("\"compressed_size\": 100"));
    }

    // ── ChunkManifest ──────────────────────────────────────────────

    #[test]
    fn test_manifest_empty() {
        let manifest = ChunkManifest::empty();
        assert!(manifest.is_empty());
        assert_eq!(manifest.chunk_count(), 0);
        assert_eq!(manifest.total_compressed(), 0);
        assert_eq!(manifest.total_uncompressed(), 0);
    }

    #[test]
    fn test_manifest_with_entries() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
            ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
        ]);

        assert_eq!(manifest.chunk_count(), 3);
        assert!(!manifest.is_empty());
        assert_eq!(manifest.total_compressed(), 75000);
        assert_eq!(manifest.total_uncompressed(), 171072);
    }

    #[test]
    fn test_manifest_hash_set() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
        ]);

        let set = manifest.hash_set();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&[0xAA; 32]));
        assert!(set.contains(&[0xBB; 32]));
        assert!(!set.contains(&[0xCC; 32]));
    }

    #[test]
    fn test_manifest_find_by_hash() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
        ]);

        let found = manifest.find_by_hash(&[0xBB; 32]);
        assert!(found.is_some());
        assert_eq!(found.unwrap().index, 1);

        assert!(manifest.find_by_hash(&[0xCC; 32]).is_none());
    }

    #[test]
    fn test_manifest_find_by_index() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
        ]);

        assert!(manifest.find_by_index(0).is_some());
        assert!(manifest.find_by_index(1).is_some());
        assert!(manifest.find_by_index(2).is_none());
    }

    #[test]
    fn test_manifest_hashes_iterator() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
        ]);

        let hashes: Vec<&[u8; 32]> = manifest.hashes().collect();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes[0], &[0xAA; 32]);
        assert_eq!(hashes[1], &[0xBB; 32]);
    }

    #[test]
    fn test_manifest_display() {
        let manifest = ChunkManifest::new(vec![ChunkManifestEntry::new(0, [0; 32], 32000, 65536)]);
        let display = format!("{}", manifest);
        assert!(display.contains("1 chunks"));
        assert!(display.contains("compressed"));
    }

    #[test]
    fn test_manifest_json_roundtrip() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ]);

        let json = serde_json::to_string(&manifest).unwrap();
        let decoded: ChunkManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, decoded);
    }

    // ── ChunkNegotiation ───────────────────────────────────────────

    #[test]
    fn test_negotiation_no_overlap() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ]);

        let have: Vec<[u8; 32]> = vec![]; // nothing
        let result = ChunkNegotiation::compute(&manifest, &have);

        assert_eq!(result.needed.len(), 2);
        assert_eq!(result.already_have, 0);
        assert_eq!(result.bytes_saved, 0);
        assert_eq!(result.bytes_needed, 60000);
        assert!(result.is_full_transfer());
        assert!(!result.is_complete());
        assert_eq!(result.total_chunks, 2);
    }

    #[test]
    fn test_negotiation_full_overlap() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ]);

        let have = vec![[0xAA; 32], [0xBB; 32]];
        let result = ChunkNegotiation::compute(&manifest, &have);

        assert!(result.needed.is_empty());
        assert_eq!(result.already_have, 2);
        assert_eq!(result.bytes_saved, 60000);
        assert_eq!(result.bytes_needed, 0);
        assert!(result.is_complete());
        assert!(!result.is_full_transfer());
    }

    #[test]
    fn test_negotiation_partial_overlap() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
            ChunkManifestEntry::new(2, [0xCC; 32], 15000, 40000),
        ]);

        let have = vec![[0xAA; 32]]; // only chunk 0
        let result = ChunkNegotiation::compute(&manifest, &have);

        assert_eq!(result.needed.len(), 2);
        assert_eq!(result.needed[0].index, 1);
        assert_eq!(result.needed[1].index, 2);
        assert_eq!(result.already_have, 1);
        assert_eq!(result.bytes_saved, 32000);
        assert_eq!(result.bytes_needed, 43000);
        assert_eq!(result.total_chunks, 3);
        assert!(!result.is_complete());
        assert!(!result.is_full_transfer());
    }

    #[test]
    fn test_negotiation_savings_pct() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 50, 100),
            ChunkManifestEntry::new(1, [0xBB; 32], 50, 100),
        ]);

        // Have one of two equal-size chunks → 50% savings
        let have = vec![[0xAA; 32]];
        let result = ChunkNegotiation::compute(&manifest, &have);
        assert!((result.savings_pct() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_negotiation_savings_pct_zero() {
        let manifest = ChunkManifest::empty();
        let result = ChunkNegotiation::compute(&manifest, &[]);
        assert!((result.savings_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_negotiation_needed_hashes() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
            ChunkManifestEntry::new(2, [0xCC; 32], 100, 200),
        ]);

        let have = vec![[0xBB; 32]];
        let result = ChunkNegotiation::compute(&manifest, &have);

        let needed = result.needed_hashes();
        assert_eq!(needed.len(), 2);
        assert!(needed.contains(&[0xAA; 32]));
        assert!(needed.contains(&[0xCC; 32]));
        assert!(!needed.contains(&[0xBB; 32]));
    }

    #[test]
    fn test_negotiation_needed_indices() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 100, 200),
            ChunkManifestEntry::new(2, [0xCC; 32], 100, 200),
        ]);

        let have = vec![[0xBB; 32]];
        let result = ChunkNegotiation::compute(&manifest, &have);

        let indices = result.needed_indices();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn test_negotiation_display() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 32000, 65536),
            ChunkManifestEntry::new(1, [0xBB; 32], 28000, 65536),
        ]);

        let have = vec![[0xAA; 32]];
        let result = ChunkNegotiation::compute(&manifest, &have);
        let display = format!("{}", result);
        assert!(display.contains("need 1 of 2"));
        assert!(display.contains("saved"));
    }

    #[test]
    fn test_negotiation_empty_manifest() {
        let manifest = ChunkManifest::empty();
        let result = ChunkNegotiation::compute(&manifest, &[[0xAA; 32]]);
        assert!(result.is_complete());
        assert_eq!(result.total_chunks, 0);
    }

    #[test]
    fn test_negotiation_extra_haves_ignored() {
        // Client claims to have chunks not in the manifest — they're just ignored
        let manifest = ChunkManifest::new(vec![ChunkManifestEntry::new(0, [0xAA; 32], 100, 200)]);

        let have = vec![[0xAA; 32], [0xFF; 32]]; // 0xFF not in manifest
        let result = ChunkNegotiation::compute(&manifest, &have);
        assert!(result.is_complete());
        assert_eq!(result.already_have, 1); // only the one that matched
    }

    #[test]
    fn test_negotiation_json_roundtrip() {
        let manifest = ChunkManifest::new(vec![
            ChunkManifestEntry::new(0, [0xAA; 32], 100, 200),
            ChunkManifestEntry::new(1, [0xBB; 32], 300, 400),
        ]);

        let result = ChunkNegotiation::compute(&manifest, &[[0xAA; 32]]);
        let json = serde_json::to_string(&result).unwrap();
        let decoded: ChunkNegotiation = serde_json::from_str(&json).unwrap();
        assert_eq!(result, decoded);
    }

    // ── StreamingPushOptions ───────────────────────────────────────

    #[test]
    fn test_push_options_default() {
        let opts = StreamingPushOptions::default();
        assert!(opts.use_delta_transfer);
        assert!(opts.report_progress);
        assert_eq!(opts.max_parallel_chunks, 4);
    }

    #[test]
    fn test_push_options_simple() {
        let opts = StreamingPushOptions::simple();
        assert!(!opts.use_delta_transfer);
        assert!(opts.report_progress);
    }

    // ── StreamingPullOptions ───────────────────────────────────────

    #[test]
    fn test_pull_options_default() {
        let opts = StreamingPullOptions::default();
        assert!(opts.layers.is_all());
        assert!(!opts.use_delta_transfer);
        assert!(opts.verify_hash);
    }

    #[test]
    fn test_pull_options_thin_pull() {
        let opts = StreamingPullOptions::thin_pull();
        assert!(opts.layers.includes(Layer::Graph));
        assert!(opts.layers.includes(Layer::Content));
        assert!(!opts.layers.includes(Layer::Semantic));
    }

    #[test]
    fn test_pull_options_thin_review() {
        let opts = StreamingPullOptions::thin_review();
        assert!(opts.layers.includes(Layer::Semantic));
        assert!(opts.layers.includes(Layer::Content));
        assert!(!opts.layers.includes(Layer::Graph));
    }

    #[test]
    fn test_pull_options_with_delta() {
        let opts = StreamingPullOptions::default().with_delta_transfer(true);
        assert!(opts.use_delta_transfer);
    }

    #[test]
    fn test_pull_options_without_verify() {
        let opts = StreamingPullOptions::default().with_verify(false);
        assert!(!opts.verify_hash);
    }

    // ── TransferProgress ───────────────────────────────────────────

    #[test]
    fn test_progress_started_display() {
        let p = TransferProgress::Started {
            total_sections: 11,
            total_bytes_estimate: 7_500_000,
        };
        let display = format!("{}", p);
        assert!(display.contains("11 sections"));
        assert!(display.contains("7.2 MB"));
    }

    #[test]
    fn test_progress_section_display() {
        let p = TransferProgress::SectionComplete {
            section: "HEADER".to_string(),
            bytes_transferred: 200,
        };
        let display = format!("{}", p);
        assert!(display.contains("HEADER"));
        assert!(display.contains("200 B"));
    }

    #[test]
    fn test_progress_chunk_display() {
        let p = TransferProgress::ChunkComplete {
            index: 3,
            bytes_transferred: 32000,
            skipped: false,
        };
        let display = format!("{}", p);
        assert!(display.contains("chunk #3"));
        assert!(display.contains("31.2 KB"));
    }

    #[test]
    fn test_progress_chunk_skipped_display() {
        let p = TransferProgress::ChunkComplete {
            index: 0,
            bytes_transferred: 0,
            skipped: true,
        };
        let display = format!("{}", p);
        assert!(display.contains("skipped"));
    }

    #[test]
    fn test_progress_finished_display() {
        let p = TransferProgress::Finished {
            total_bytes: 7_500_000,
            elapsed_ms: 3200,
        };
        let display = format!("{}", p);
        assert!(display.contains("complete"));
        assert!(display.contains("3.2s"));
    }

    // ── TransferStats ──────────────────────────────────────────────

    #[test]
    fn test_transfer_stats_default() {
        let stats = TransferStats::default();
        assert_eq!(stats.total_chunks(), 0);
        assert_eq!(stats.total_bytes(), 0);
        assert!((stats.savings_pct() - 0.0).abs() < f64::EPSILON);
        assert_eq!(stats.bytes_per_second(), 0);
    }

    #[test]
    fn test_transfer_stats_with_values() {
        let stats = TransferStats {
            sections_transferred: 11,
            chunks_transferred: 5,
            chunks_skipped: 3,
            bytes_transferred: 75000,
            bytes_skipped: 60000,
            elapsed_ms: 1200,
        };

        assert_eq!(stats.total_chunks(), 8);
        assert_eq!(stats.total_bytes(), 135000);
        let savings = stats.savings_pct();
        assert!(savings > 44.0 && savings < 45.0);
        assert!(stats.bytes_per_second() > 0);
    }

    #[test]
    fn test_transfer_stats_display_no_skipped() {
        let stats = TransferStats {
            sections_transferred: 5,
            chunks_transferred: 3,
            chunks_skipped: 0,
            bytes_transferred: 50000,
            bytes_skipped: 0,
            elapsed_ms: 500,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 sections"));
        assert!(display.contains("3 chunks"));
        assert!(!display.contains("skipped"));
    }

    #[test]
    fn test_transfer_stats_display_with_skipped() {
        let stats = TransferStats {
            sections_transferred: 8,
            chunks_transferred: 2,
            chunks_skipped: 5,
            bytes_transferred: 20000,
            bytes_skipped: 80000,
            elapsed_ms: 300,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 skipped"));
        assert!(display.contains("savings"));
    }

    #[test]
    fn test_transfer_stats_bytes_per_second() {
        let stats = TransferStats {
            bytes_transferred: 1_000_000,
            elapsed_ms: 1000, // 1 second
            ..Default::default()
        };
        assert_eq!(stats.bytes_per_second(), 1_000_000);
    }

    #[test]
    fn test_transfer_stats_bytes_per_second_zero_elapsed() {
        let stats = TransferStats {
            bytes_transferred: 1_000_000,
            elapsed_ms: 0,
            ..Default::default()
        };
        assert_eq!(stats.bytes_per_second(), 0);
    }

    // ── hex_hash serde ─────────────────────────────────────────────

    #[test]
    fn test_hex_hash_roundtrip_via_entry() {
        let hash = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xAB, 0xCD, 0xEF,
        ];

        let entry = ChunkManifestEntry::new(0, hash, 100, 200);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));

        let decoded: ChunkManifestEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.hash, hash);
    }

    #[test]
    fn test_hex_hash_invalid_length() {
        let json = r#"{"index":0,"hash":"aabb","compressed_size":0,"uncompressed_size":0}"#;
        let result: Result<ChunkManifestEntry, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_hex_hash_invalid_chars() {
        let json = r#"{"index":0,"hash":"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz","compressed_size":0,"uncompressed_size":0}"#;
        let result: Result<ChunkManifestEntry, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // ── format_size ────────────────────────────────────────────────

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(32000), "31.2 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(7_500_000), "7.2 MB");
    }

    // ── Integration: realistic delta push scenario ─────────────────

    #[test]
    fn test_realistic_delta_push_scenario() {
        // Scenario: Client edits 1 line in a 10 MB file.
        // The file has ~150 chunks (10 MB / 64 KB avg).
        // The edit changes 1 chunk. All other chunks are identical.

        let mut entries = Vec::new();
        for i in 0..150 {
            let mut hash = [0u8; 32];
            hash[0] = (i / 256) as u8;
            hash[1] = (i % 256) as u8;
            entries.push(ChunkManifestEntry::new(
                i as u32, hash, 32000, // ~32 KB compressed
                65536, // 64 KB uncompressed
            ));
        }

        // The edit changed chunk 75
        let mut edited_entries = entries.clone();
        edited_entries[75].hash = [0xFF; 32]; // different hash

        let manifest = ChunkManifest::new(edited_entries);

        // Server has all the original chunks
        let server_hashes: Vec<[u8; 32]> = entries.iter().map(|e| e.hash).collect();

        let negotiation = ChunkNegotiation::compute(&manifest, &server_hashes);

        // Only 1 chunk should need transferring
        assert_eq!(negotiation.needed.len(), 1);
        assert_eq!(negotiation.needed[0].index, 75);
        assert_eq!(negotiation.already_have, 149);
        assert_eq!(negotiation.bytes_needed, 32000);
        assert_eq!(negotiation.bytes_saved, 149 * 32000);

        // Savings should be ~99.3%
        assert!(negotiation.savings_pct() > 99.0);

        println!("Delta push scenario: {}", negotiation);
        println!(
            "  Transfer {} instead of {}",
            format_size(negotiation.bytes_needed),
            format_size(manifest.total_compressed()),
        );
    }

    #[test]
    fn test_realistic_clone_scenario() {
        // Scenario: Fresh clone — client has nothing.
        let entries: Vec<ChunkManifestEntry> = (0..50)
            .map(|i| {
                let mut hash = [0u8; 32];
                hash[0] = i;
                ChunkManifestEntry::new(i as u32, hash, 30000, 65536)
            })
            .collect();

        let manifest = ChunkManifest::new(entries);

        // Client has nothing
        let negotiation = ChunkNegotiation::compute(&manifest, &[]);

        assert!(negotiation.is_full_transfer());
        assert_eq!(negotiation.needed.len(), 50);
        assert_eq!(negotiation.bytes_needed, 50 * 30000);
        assert!((negotiation.savings_pct() - 0.0).abs() < f64::EPSILON);
    }
}

//! Layer selection and chunk manifest types.
//!
//! Contains [`LayerSelection`] for specifying which layers to include in a
//! transfer, and [`ChunkManifest`] for describing the content chunks in a change.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

use super::types::{format_size, ChunkManifestEntry, Layer};

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

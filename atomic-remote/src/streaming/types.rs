//! Core protocol types for the streaming push/pull protocol.
//!
//! Contains the fundamental types used across the streaming module:
//! - [`Layer`] — individual protocol layers (Graph, Semantic, Content)
//! - [`ChunkManifestEntry`] — one entry in a chunk manifest
//! - [`hex_hash`] — serde module for hex-encoding `[u8; 32]`
//! - [`format_size`] — human-readable byte size formatting

use serde::{Deserialize, Serialize};
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
// ChunkManifestEntry — one entry in the chunk manifest
// ═══════════════════════════════════════════════════════════════════════

/// A single entry in a [`ChunkManifest`](super::ChunkManifest).
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
// Helper: hex serialization for [u8; 32]
// ═══════════════════════════════════════════════════════════════════════

/// Serde module for hex-encoding [u8; 32] in JSON.
///
/// Serializes as a 64-character lowercase hex string, deserializes from
/// the same. This makes chunk manifests human-readable in JSON.
pub mod hex_hash {
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
pub(super) fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

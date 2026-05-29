//! Section payload types for the Change Format V3.
//!
//! This module defines the typed data structures that go **inside** each
//! independently-compressed section of a V3 change file. These are the
//! bridge between the generic writer/reader (which handle opaque `&[u8]`
//! payloads) and the domain-specific graph/semantic operations.
//!
//! # Section Payload Types
//!
//! | Section Type | Payload Type | Contains |
//! |-------------|-------------|----------|
//! | GRAPH | [`GraphSectionPayload`] | Compact graph ops for one file |
//! | SEMANTIC | [`SemanticSectionPayload`] | FileOps (Trunk/Branch/Leaf) for one file |
//!
//! # Architecture
//!
//! ```text
//! Per-File Section Architecture
//!
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  GRAPH Section (file: src/main.rs)                              │
//! │  ┌───────────────────────────────────────────────────────────┐  │
//! │  │  GraphSectionPayload {                                    │  │
//! │  │    path: "src/main.rs",                                   │  │
//! │  │    ops: [CompactGraphOp::FileAdd { ... }],                │  │
//! │  │    content_range: 0..1024,                                │  │
//! │  │  }                                                        │  │
//! │  └───────────────────────────────────────────────────────────┘  │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  SEMANTIC Section (file: src/main.rs)                           │
//! │  ┌───────────────────────────────────────────────────────────┐  │
//! │  │  SemanticSectionPayload {                                 │  │
//! │  │    path: "src/main.rs",                                   │  │
//! │  │    file_ops: FileOps { trunk_op: Create, lines: [...] },  │  │
//! │  │    content_range: 0..1024,                                │  │
//! │  │  }                                                        │  │
//! │  └───────────────────────────────────────────────────────────┘  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Relationship Between GRAPH and SEMANTIC Sections
//!
//! Each modified file in a change can have **two** sections:
//!
//! - **GRAPH section**: The storage/merge layer. Contains [`CompactGraphOp`]s that
//!   modify the repository DAG (vertex insertions, edge updates). This is the
//!   minimum required to apply a change. A "thin pull" downloads only these.
//!
//! - **SEMANTIC section**: The display/analysis layer. Contains [`FileOps`] with
//!   Trunk/Branch/Leaf operations for line-level and token-level diffs, blame,
//!   and code review. This is optional — it can be regenerated from graph + content.
//!   A "thin review" downloads only these + content chunks.
//!
//! Both sections for the same file share the same `content_range`, which indexes
//! into the change's content chunks. This enables the reader to correlate
//! GRAPH and SEMANTIC sections for the same file by matching their `path` fields.
//!
//! # Serialization
//!
//! All payload types derive `serde::Serialize` and `serde::Deserialize` and are
//! designed for postcard encoding. The writer serializes them with
//! `postcard::to_allocvec()`, compresses with zstd, and writes as a section.
//! The reader reverses this: decompress, then `postcard::from_bytes()`.
//!
//! # Examples
//!
//! ## Writing a GRAPH section
//!
//! ```rust
//! use atomic_core::change::format_v3::sections::GraphSectionPayload;
//! use atomic_core::change::format_v3::compact::CompactGraphOp;
//!
//! let payload = GraphSectionPayload {
//!     path: "src/main.rs".to_string(),
//!     ops: vec![], // CompactGraphOps would go here
//!     content_start: 0,
//!     content_end: 1024,
//! };
//!
//! // Serialize with postcard for the writer
//! let bytes = postcard::to_allocvec(&payload).unwrap();
//! assert!(!bytes.is_empty());
//!
//! // Deserialize on the reader side
//! let decoded: GraphSectionPayload = postcard::from_bytes(&bytes).unwrap();
//! assert_eq!(decoded.path, "src/main.rs");
//! ```
//!
//! ## Writing a SEMANTIC section
//!
//! ```rust
//! use atomic_core::change::format_v3::sections::SemanticSectionPayload;
//! use atomic_core::change::ops::FileOps;
//! use atomic_core::crdt::TrunkId;
//! use atomic_core::change::Encoding;
//! use atomic_core::types::NodeId;
//!
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//! let file_ops = FileOps::create(trunk_id, "src/main.rs".to_string(), Some(Encoding::Utf8));
//!
//! let payload = SemanticSectionPayload {
//!     path: "src/main.rs".to_string(),
//!     file_ops,
//!     content_start: 0,
//!     content_end: 1024,
//! };
//!
//! let bytes = postcard::to_allocvec(&payload).unwrap();
//! let decoded: SemanticSectionPayload = postcard::from_bytes(&bytes).unwrap();
//! assert_eq!(decoded.path, "src/main.rs");
//! ```

use super::compact::CompactGraphOp;
use super::error::FormatResult;
use crate::change::ops::FileOps;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

// ═══════════════════════════════════════════════════════════════════════
// GraphSectionPayload — what goes inside a GRAPH section
// ═══════════════════════════════════════════════════════════════════════

/// Payload for a GRAPH section in a V3 change file.
///
/// Each modified file gets one GRAPH section containing all the graph
/// operations (vertex insertions, edge updates) needed to apply the
/// change to that file's DAG. This is the **storage/merge layer**.
///
/// # Fields
///
/// - `path`: The file path (relative to repo root). Used to correlate
///   GRAPH and SEMANTIC sections for the same file.
/// - `ops`: The compact graph operations for this file. Uses
///   [`CompactGraphOp`] with hash-indexed positions instead of full hashes.
/// - `content_start`, `content_end`: Byte range into the change's content
///   chunks. The actual file content is stored separately in CONTENT
///   sections; this range tells the reader which content bytes belong
///   to this file's graph operations.
///
/// # Serialization Size
///
/// A typical GRAPH section for a file edit contains:
/// - Path string: ~20 bytes (postcard length-prefixed)
/// - 1-5 CompactGraphOps: 15-200 bytes each
/// - Content range: 2-10 bytes (two varints)
///
/// After zstd compression, a typical section is 50-500 bytes — compared
/// to 2-20 KB for the same data in V2 bincode format.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::sections::GraphSectionPayload;
///
/// let payload = GraphSectionPayload::new(
///     "src/lib.rs".to_string(),
///     vec![], // ops
///     0,      // content_start
///     500,    // content_end
/// );
///
/// assert_eq!(payload.path(), "src/lib.rs");
/// assert_eq!(payload.content_range(), 0..500);
/// assert!(payload.ops().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSectionPayload {
    /// File path (relative to repository root).
    ///
    /// This is the same path stored in the corresponding SEMANTIC section
    /// and in the `GraphOp::path()` of the contained operations.
    pub path: String,

    /// Compact graph operations for this file.
    ///
    /// These are the storage-layer operations that modify the repository
    /// DAG. Typically contains one or more of:
    /// - `FileAdd` (new file with content)
    /// - `Edit` (insert/delete content)
    /// - `Replacement` (delete + insert)
    /// - `FileDel`, `FileUndel`, `FileMove`, etc.
    pub ops: Vec<CompactGraphOp>,

    /// Start of the byte range in the content chunks (inclusive).
    ///
    /// Together with `content_end`, this identifies which bytes in the
    /// change's content blob belong to this file's operations.
    /// For files with no content (e.g., empty directories), this equals
    /// `content_end` (zero-length range).
    pub content_start: u64,

    /// End of the byte range in the content chunks (exclusive).
    ///
    /// `content_end - content_start` gives the number of content bytes
    /// associated with this file's graph operations.
    pub content_end: u64,
}

impl GraphSectionPayload {
    /// Create a new graph section payload.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (relative to repository root).
    /// * `ops` - Compact graph operations for this file.
    /// * `content_start` - Start of content range (inclusive).
    /// * `content_end` - End of content range (exclusive).
    pub fn new(
        path: String,
        ops: Vec<CompactGraphOp>,
        content_start: u64,
        content_end: u64,
    ) -> Self {
        Self {
            path,
            ops,
            content_start,
            content_end,
        }
    }

    /// Returns the file path.
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns a reference to the compact graph operations.
    #[inline]
    pub fn ops(&self) -> &[CompactGraphOp] {
        &self.ops
    }

    /// Returns the number of graph operations.
    #[inline]
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Returns the content byte range as a `Range<u64>`.
    ///
    /// This range indexes into the change's content blob (the concatenation
    /// of all CONTENT chunks).
    #[inline]
    pub fn content_range(&self) -> Range<u64> {
        self.content_start..self.content_end
    }

    /// Returns the content size in bytes.
    #[inline]
    pub fn content_len(&self) -> u64 {
        self.content_end.saturating_sub(self.content_start)
    }

    /// Returns `true` if this file has no associated content.
    ///
    /// This is common for structural operations like `DirAdd` or `DirDel`
    /// where only edges are modified, not content.
    #[inline]
    pub fn has_content(&self) -> bool {
        self.content_end > self.content_start
    }

    /// Returns `true` if this section has no operations.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Serialize this payload to postcard bytes.
    ///
    /// This is a convenience wrapper around [`postcard::to_allocvec`].
    /// The caller typically compresses the result with zstd before
    /// passing it to the writer.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::FormatError::Postcard`] if serialization fails.
    pub fn to_postcard_bytes(&self) -> FormatResult<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserialize a payload from postcard bytes.
    ///
    /// This is a convenience wrapper around [`postcard::from_bytes`].
    /// The caller typically decompresses the section data with zstd
    /// before calling this.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::FormatError::Postcard`] if deserialization fails.
    pub fn from_postcard_bytes(bytes: &[u8]) -> FormatResult<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// Consume this payload and return its parts.
    ///
    /// Returns `(path, ops, content_start, content_end)`.
    pub fn into_parts(self) -> (String, Vec<CompactGraphOp>, u64, u64) {
        (self.path, self.ops, self.content_start, self.content_end)
    }
}

impl fmt::Display for GraphSectionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GraphSection({}, {} ops, content {}..{})",
            self.path,
            self.ops.len(),
            self.content_start,
            self.content_end,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SemanticSectionPayload — what goes inside a SEMANTIC section
// ═══════════════════════════════════════════════════════════════════════

/// Payload for a SEMANTIC section in a V3 change file.
///
/// Each modified file can have one SEMANTIC section containing the
/// Trunk/Branch/Leaf operations for line-level and token-level diffs,
/// blame, and code review. This is the **display/analysis layer**.
///
/// # Fields
///
/// - `path`: The file path (must match the corresponding GRAPH section).
/// - `file_ops`: The semantic operations organized as Trunk → Branch → Leaf.
///   These represent the human-readable changes:
///   - **Trunk (File)**: Create, delete, move, undelete
///   - **Branch (Line)**: Insert, delete, restore
///   - **Leaf (Token)**: Insert, delete, replace (within a line)
/// - `content_start`, `content_end`: Same content range as the corresponding
///   GRAPH section (they reference the same content bytes).
///
/// # Optional Nature
///
/// SEMANTIC sections are **optional**. A "thin pull" omits them entirely.
/// They can be regenerated from graph + content at any time using the
/// tokenizer. This is the key enabler for:
///
/// - **Thin pull**: Download only GRAPH + CONTENT sections, reconstruct
///   SEMANTIC locally on demand.
/// - **Tokenizer upgrades**: When the tokenizer improves, regenerate
///   SEMANTIC sections without touching GRAPH or CONTENT.
/// - **AST enrichment**: The server can add tree-sitter AST node types
///   to SEMANTIC sections without the client needing tree-sitter.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::sections::SemanticSectionPayload;
/// use atomic_core::change::ops::FileOps;
/// use atomic_core::crdt::TrunkId;
/// use atomic_core::change::Encoding;
/// use atomic_core::types::NodeId;
///
/// let trunk_id = TrunkId::new(NodeId::new(1), 0);
/// let file_ops = FileOps::create(trunk_id, "README.md".to_string(), Some(Encoding::Utf8));
///
/// let payload = SemanticSectionPayload::new(
///     "README.md".to_string(),
///     file_ops,
///     0,
///     256,
/// );
///
/// assert_eq!(payload.path(), "README.md");
/// assert!(payload.file_ops().is_create());
/// assert_eq!(payload.content_len(), 256);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSectionPayload {
    /// File path (relative to repository root).
    ///
    /// This MUST match the `path` field of the corresponding GRAPH section.
    /// The reader uses the path to correlate GRAPH and SEMANTIC sections.
    pub path: String,

    /// Semantic operations for this file (Trunk/Branch/Leaf hierarchy).
    ///
    /// Contains:
    /// - `TrunkOp`: File-level operation (create, delete, move, undelete, edit)
    /// - `Vec<LineOps>`: Line-level operations with optional token-level detail
    pub file_ops: FileOps,

    /// Start of the byte range in the content chunks (inclusive).
    ///
    /// Same range as the corresponding GRAPH section — they reference
    /// the same content bytes.
    pub content_start: u64,

    /// End of the byte range in the content chunks (exclusive).
    pub content_end: u64,
}

impl SemanticSectionPayload {
    /// Create a new semantic section payload.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (relative to repository root).
    /// * `file_ops` - Semantic operations for this file.
    /// * `content_start` - Start of content range (inclusive).
    /// * `content_end` - End of content range (exclusive).
    pub fn new(path: String, file_ops: FileOps, content_start: u64, content_end: u64) -> Self {
        Self {
            path,
            file_ops,
            content_start,
            content_end,
        }
    }

    /// Returns the file path.
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns a reference to the semantic operations.
    #[inline]
    pub fn file_ops(&self) -> &FileOps {
        &self.file_ops
    }

    /// Returns the content byte range as a `Range<u64>`.
    #[inline]
    pub fn content_range(&self) -> Range<u64> {
        self.content_start..self.content_end
    }

    /// Returns the content size in bytes.
    #[inline]
    pub fn content_len(&self) -> u64 {
        self.content_end.saturating_sub(self.content_start)
    }

    /// Returns `true` if this file has no associated content.
    #[inline]
    pub fn has_content(&self) -> bool {
        self.content_end > self.content_start
    }

    /// Returns the number of line operations in this section.
    #[inline]
    pub fn line_count(&self) -> usize {
        self.file_ops.line_count()
    }

    /// Returns the total number of token operations across all lines.
    #[inline]
    pub fn token_count(&self) -> usize {
        self.file_ops.token_count()
    }

    /// Serialize this payload to postcard bytes.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::FormatError::Postcard`] if serialization fails.
    pub fn to_postcard_bytes(&self) -> FormatResult<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserialize a payload from postcard bytes.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::FormatError::Postcard`] if deserialization fails.
    pub fn from_postcard_bytes(bytes: &[u8]) -> FormatResult<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// Consume this payload and return its parts.
    ///
    /// Returns `(path, file_ops, content_start, content_end)`.
    pub fn into_parts(self) -> (String, FileOps, u64, u64) {
        (
            self.path,
            self.file_ops,
            self.content_start,
            self.content_end,
        )
    }
}

impl fmt::Display for SemanticSectionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SemanticSection({}, {} lines, {} tokens, content {}..{})",
            self.path,
            self.line_count(),
            self.token_count(),
            self.content_start,
            self.content_end,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// SectionPair — convenience type for correlated GRAPH + SEMANTIC
// ═══════════════════════════════════════════════════════════════════════

/// A correlated pair of GRAPH and SEMANTIC sections for the same file.
///
/// This is a convenience type used when building both section types
/// during recording. The `path` and content range are shared.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::sections::SectionPair;
///
/// let pair = SectionPair::graph_only(
///     "src/lib.rs".to_string(),
///     vec![],
///     0,
///     100,
/// );
///
/// assert!(pair.has_graph());
/// assert!(!pair.has_semantic());
/// ```
#[derive(Clone, Debug)]
pub struct SectionPair {
    /// File path (shared by both sections).
    pub path: String,

    /// Graph section payload (always present).
    pub graph: Option<GraphSectionPayload>,

    /// Semantic section payload (optional — omitted for thin changes).
    pub semantic: Option<SemanticSectionPayload>,
}

impl SectionPair {
    /// Create a pair with both GRAPH and SEMANTIC sections.
    ///
    /// # Arguments
    ///
    /// * `path` - File path (used in both sections).
    /// * `ops` - Compact graph operations.
    /// * `file_ops` - Semantic operations.
    /// * `content_start` - Start of content range.
    /// * `content_end` - End of content range.
    pub fn both(
        path: String,
        ops: Vec<CompactGraphOp>,
        file_ops: FileOps,
        content_start: u64,
        content_end: u64,
    ) -> Self {
        Self {
            graph: Some(GraphSectionPayload::new(
                path.clone(),
                ops,
                content_start,
                content_end,
            )),
            semantic: Some(SemanticSectionPayload::new(
                path.clone(),
                file_ops,
                content_start,
                content_end,
            )),
            path,
        }
    }

    /// Create a pair with only a GRAPH section (no semantic layer).
    ///
    /// This is used for thin changes or when the semantic layer will
    /// be regenerated later.
    pub fn graph_only(
        path: String,
        ops: Vec<CompactGraphOp>,
        content_start: u64,
        content_end: u64,
    ) -> Self {
        Self {
            graph: Some(GraphSectionPayload::new(
                path.clone(),
                ops,
                content_start,
                content_end,
            )),
            semantic: None,
            path,
        }
    }

    /// Create a pair with only a SEMANTIC section (no graph layer).
    ///
    /// This is unusual but can happen when enriching an existing change
    /// with semantic metadata (e.g., server-side AST enrichment).
    pub fn semantic_only(
        path: String,
        file_ops: FileOps,
        content_start: u64,
        content_end: u64,
    ) -> Self {
        Self {
            graph: None,
            semantic: Some(SemanticSectionPayload::new(
                path.clone(),
                file_ops,
                content_start,
                content_end,
            )),
            path,
        }
    }

    /// Returns `true` if this pair has a GRAPH section.
    #[inline]
    pub fn has_graph(&self) -> bool {
        self.graph.is_some()
    }

    /// Returns `true` if this pair has a SEMANTIC section.
    #[inline]
    pub fn has_semantic(&self) -> bool {
        self.semantic.is_some()
    }

    /// Returns `true` if this pair has both GRAPH and SEMANTIC sections.
    #[inline]
    pub fn has_both(&self) -> bool {
        self.graph.is_some() && self.semantic.is_some()
    }

    /// Returns the file path.
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for SectionPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let layers = match (self.has_graph(), self.has_semantic()) {
            (true, true) => "GRAPH+SEMANTIC",
            (true, false) => "GRAPH",
            (false, true) => "SEMANTIC",
            (false, false) => "(empty)",
        };
        write!(f, "SectionPair({}, {})", self.path, layers)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::format_v3::compact::{CompactAtom, CompactInsertion};
    use crate::change::format_v3::types::CompactPosition;
    use crate::change::Encoding;
    use crate::crdt::TrunkId;
    use crate::types::{EdgeFlags, NodeId};

    // ── GraphSectionPayload ────────────────────────────────────────

    #[test]
    fn test_graph_section_new() {
        let payload = GraphSectionPayload::new("src/main.rs".to_string(), vec![], 0, 1024);

        assert_eq!(payload.path(), "src/main.rs");
        assert!(payload.ops().is_empty());
        assert_eq!(payload.op_count(), 0);
        assert_eq!(payload.content_range(), 0..1024);
        assert_eq!(payload.content_len(), 1024);
        assert!(payload.has_content());
        assert!(payload.is_empty());
    }

    #[test]
    fn test_graph_section_with_ops() {
        let op = CompactGraphOp::Edit {
            change: CompactAtom::Insertion(CompactInsertion {
                predecessors: vec![CompactPosition::self_ref(0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 100,
                end: 150,
                inode: CompactPosition::self_ref(0),
            }),
            local: crate::change::Local::new("test.rs", 10),
            encoding: Some(Encoding::Utf8),
        };

        let payload = GraphSectionPayload::new("test.rs".to_string(), vec![op], 100, 150);

        assert_eq!(payload.op_count(), 1);
        assert!(!payload.is_empty());
        assert_eq!(payload.content_len(), 50);
    }

    #[test]
    fn test_graph_section_no_content() {
        let payload = GraphSectionPayload::new("empty_dir/".to_string(), vec![], 0, 0);

        assert!(!payload.has_content());
        assert_eq!(payload.content_len(), 0);
        assert_eq!(payload.content_range(), 0..0);
    }

    #[test]
    fn test_graph_section_display() {
        let payload = GraphSectionPayload::new("src/lib.rs".to_string(), vec![], 0, 500);
        let display = format!("{}", payload);
        assert!(display.contains("src/lib.rs"));
        assert!(display.contains("0 ops"));
        assert!(display.contains("0..500"));
    }

    #[test]
    fn test_graph_section_into_parts() {
        let payload = GraphSectionPayload::new("a.rs".to_string(), vec![], 10, 20);
        let (path, ops, start, end) = payload.into_parts();
        assert_eq!(path, "a.rs");
        assert!(ops.is_empty());
        assert_eq!(start, 10);
        assert_eq!(end, 20);
    }

    #[test]
    fn test_graph_section_postcard_roundtrip() {
        let payload = GraphSectionPayload::new("src/main.rs".to_string(), vec![], 0, 1024);

        let bytes = payload.to_postcard_bytes().unwrap();
        assert!(!bytes.is_empty());

        let decoded = GraphSectionPayload::from_postcard_bytes(&bytes).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn test_graph_section_postcard_roundtrip_with_ops() {
        let op = CompactGraphOp::Edit {
            change: CompactAtom::Insertion(CompactInsertion {
                predecessors: vec![CompactPosition::new(1, 50)],
                successors: vec![CompactPosition::self_ref(100)],
                flag: EdgeFlags::BLOCK.bits(),
                start: 200,
                end: 300,
                inode: CompactPosition::self_ref(0),
            }),
            local: crate::change::Local::new("lib.rs", 42),
            encoding: Some(Encoding::Utf8),
        };

        let payload = GraphSectionPayload::new("lib.rs".to_string(), vec![op], 200, 300);

        let bytes = payload.to_postcard_bytes().unwrap();
        let decoded = GraphSectionPayload::from_postcard_bytes(&bytes).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn test_graph_section_postcard_deserialization_error() {
        let bad_bytes = [0xFF, 0xFF, 0xFF, 0xFF];
        let result = GraphSectionPayload::from_postcard_bytes(&bad_bytes);
        assert!(result.is_err());
    }

    // ── SemanticSectionPayload ─────────────────────────────────────

    #[test]
    fn test_semantic_section_new() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "hello.rs".to_string(), Some(Encoding::Utf8));

        let payload = SemanticSectionPayload::new("hello.rs".to_string(), file_ops, 0, 256);

        assert_eq!(payload.path(), "hello.rs");
        assert!(payload.file_ops().is_create());
        assert_eq!(payload.content_range(), 0..256);
        assert_eq!(payload.content_len(), 256);
        assert!(payload.has_content());
    }

    #[test]
    fn test_semantic_section_line_and_token_count() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "test.rs".to_string(), Some(Encoding::Utf8));

        // create returns an empty FileOps with no line ops
        let payload = SemanticSectionPayload::new("test.rs".to_string(), file_ops, 0, 100);

        assert_eq!(payload.line_count(), 0);
        assert_eq!(payload.token_count(), 0);
    }

    #[test]
    fn test_semantic_section_no_content() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::delete(trunk_id, "gone.rs".to_string());

        let payload = SemanticSectionPayload::new("gone.rs".to_string(), file_ops, 0, 0);

        assert!(!payload.has_content());
        assert_eq!(payload.content_len(), 0);
    }

    #[test]
    fn test_semantic_section_display() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "main.rs".to_string(), Some(Encoding::Utf8));

        let payload = SemanticSectionPayload::new("main.rs".to_string(), file_ops, 0, 500);
        let display = format!("{}", payload);
        assert!(display.contains("main.rs"));
        assert!(display.contains("0 lines"));
        assert!(display.contains("0..500"));
    }

    #[test]
    fn test_semantic_section_into_parts() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "x.rs".to_string(), None);

        let payload = SemanticSectionPayload::new("x.rs".to_string(), file_ops.clone(), 5, 15);
        let (path, ops, start, end) = payload.into_parts();
        assert_eq!(path, "x.rs");
        assert_eq!(ops, file_ops);
        assert_eq!(start, 5);
        assert_eq!(end, 15);
    }

    #[test]
    fn test_semantic_section_postcard_roundtrip() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "roundtrip.rs".to_string(), Some(Encoding::Utf8));

        let payload = SemanticSectionPayload::new("roundtrip.rs".to_string(), file_ops, 100, 200);

        let bytes = payload.to_postcard_bytes().unwrap();
        assert!(!bytes.is_empty());

        let decoded = SemanticSectionPayload::from_postcard_bytes(&bytes).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn test_semantic_section_postcard_deserialization_error() {
        let bad_bytes = [0xFF, 0xFF, 0xFF, 0xFF];
        let result = SemanticSectionPayload::from_postcard_bytes(&bad_bytes);
        assert!(result.is_err());
    }

    // ── SectionPair ────────────────────────────────────────────────

    #[test]
    fn test_section_pair_both() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "both.rs".to_string(), Some(Encoding::Utf8));

        let pair = SectionPair::both("both.rs".to_string(), vec![], file_ops, 0, 100);

        assert!(pair.has_graph());
        assert!(pair.has_semantic());
        assert!(pair.has_both());
        assert_eq!(pair.path(), "both.rs");

        let graph = pair.graph.as_ref().unwrap();
        assert_eq!(graph.path(), "both.rs");
        assert_eq!(graph.content_range(), 0..100);

        let semantic = pair.semantic.as_ref().unwrap();
        assert_eq!(semantic.path(), "both.rs");
        assert_eq!(semantic.content_range(), 0..100);
    }

    #[test]
    fn test_section_pair_graph_only() {
        let pair = SectionPair::graph_only("graph_only.rs".to_string(), vec![], 0, 50);

        assert!(pair.has_graph());
        assert!(!pair.has_semantic());
        assert!(!pair.has_both());
        assert_eq!(pair.path(), "graph_only.rs");
    }

    #[test]
    fn test_section_pair_semantic_only() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "sem.rs".to_string(), None);

        let pair = SectionPair::semantic_only("sem.rs".to_string(), file_ops, 0, 10);

        assert!(!pair.has_graph());
        assert!(pair.has_semantic());
        assert!(!pair.has_both());
    }

    #[test]
    fn test_section_pair_display_both() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "x.rs".to_string(), None);
        let pair = SectionPair::both("x.rs".to_string(), vec![], file_ops, 0, 0);

        let display = format!("{}", pair);
        assert!(display.contains("x.rs"));
        assert!(display.contains("GRAPH+SEMANTIC"));
    }

    #[test]
    fn test_section_pair_display_graph_only() {
        let pair = SectionPair::graph_only("y.rs".to_string(), vec![], 0, 0);
        let display = format!("{}", pair);
        assert!(display.contains("GRAPH"));
        assert!(!display.contains("SEMANTIC"));
    }

    #[test]
    fn test_section_pair_display_semantic_only() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "z.rs".to_string(), None);
        let pair = SectionPair::semantic_only("z.rs".to_string(), file_ops, 0, 0);

        let display = format!("{}", pair);
        assert!(display.contains("SEMANTIC"));
    }

    // ── Cross-Section Path Correlation ─────────────────────────────

    #[test]
    fn test_graph_and_semantic_paths_match() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "src/main.rs".to_string(), Some(Encoding::Utf8));

        let pair = SectionPair::both("src/main.rs".to_string(), vec![], file_ops, 0, 1000);

        assert_eq!(
            pair.graph.as_ref().unwrap().path(),
            pair.semantic.as_ref().unwrap().path(),
        );
    }

    #[test]
    fn test_graph_and_semantic_content_ranges_match() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "lib.rs".to_string(), None);

        let pair = SectionPair::both("lib.rs".to_string(), vec![], file_ops, 500, 1500);

        assert_eq!(
            pair.graph.as_ref().unwrap().content_range(),
            pair.semantic.as_ref().unwrap().content_range(),
        );
    }

    // ── Content Range Edge Cases ───────────────────────────────────

    #[test]
    fn test_graph_section_zero_length_content() {
        let payload = GraphSectionPayload::new("empty.rs".to_string(), vec![], 42, 42);
        assert!(!payload.has_content());
        assert_eq!(payload.content_len(), 0);
    }

    #[test]
    fn test_graph_section_large_content_range() {
        let payload = GraphSectionPayload::new("big.bin".to_string(), vec![], 0, 10_000_000);
        assert!(payload.has_content());
        assert_eq!(payload.content_len(), 10_000_000);
    }

    #[test]
    fn test_content_range_u64_max() {
        // Verify we handle large u64 values gracefully
        let payload =
            GraphSectionPayload::new("huge.dat".to_string(), vec![], u64::MAX - 100, u64::MAX);
        assert_eq!(payload.content_len(), 100);
    }

    // ── Postcard Size Comparison ───────────────────────────────────

    #[test]
    fn test_graph_section_postcard_is_compact() {
        let payload = GraphSectionPayload::new("src/main.rs".to_string(), vec![], 0, 1024);

        let postcard_bytes = payload.to_postcard_bytes().unwrap();

        // A graph section with no ops and a short path should be very small:
        // - path "src/main.rs" (11 chars): ~12 bytes (length prefix + chars)
        // - empty ops vec: 1 byte (length 0)
        // - content_start 0: 1 byte (varint)
        // - content_end 1024: 2 bytes (varint)
        // Total: ~16 bytes
        assert!(
            postcard_bytes.len() < 30,
            "empty graph section should be < 30 bytes, got {}",
            postcard_bytes.len(),
        );
    }

    #[test]
    fn test_semantic_section_postcard_is_compact() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "test.rs".to_string(), Some(Encoding::Utf8));

        let payload = SemanticSectionPayload::new("test.rs".to_string(), file_ops, 0, 100);

        let postcard_bytes = payload.to_postcard_bytes().unwrap();

        // A semantic section with a Create op and no lines should be small
        assert!(
            postcard_bytes.len() < 80,
            "simple semantic section should be < 80 bytes, got {}",
            postcard_bytes.len(),
        );
    }

    // ── Multiple Files Scenario ────────────────────────────────────

    #[test]
    fn test_multiple_graph_sections_serialize_independently() {
        let sections = vec![
            GraphSectionPayload::new("src/a.rs".to_string(), vec![], 0, 100),
            GraphSectionPayload::new("src/b.rs".to_string(), vec![], 100, 250),
            GraphSectionPayload::new("src/c.rs".to_string(), vec![], 250, 300),
        ];

        // Each section serializes independently — no shared state
        let serialized: Vec<Vec<u8>> = sections
            .iter()
            .map(|s| s.to_postcard_bytes().unwrap())
            .collect();

        let deserialized: Vec<GraphSectionPayload> = serialized
            .iter()
            .map(|b| GraphSectionPayload::from_postcard_bytes(b).unwrap())
            .collect();

        assert_eq!(sections, deserialized);
    }

    #[test]
    fn test_content_ranges_are_contiguous_for_multi_file() {
        // In a typical change, content ranges are contiguous across files
        let pairs = [
            GraphSectionPayload::new("a.rs".to_string(), vec![], 0, 100),
            GraphSectionPayload::new("b.rs".to_string(), vec![], 100, 350),
            GraphSectionPayload::new("c.rs".to_string(), vec![], 350, 400),
        ];

        // Verify ranges don't overlap and are contiguous
        for i in 1..pairs.len() {
            assert_eq!(
                pairs[i - 1].content_end,
                pairs[i].content_start,
                "content ranges should be contiguous between {} and {}",
                pairs[i - 1].path(),
                pairs[i].path(),
            );
        }
    }
}

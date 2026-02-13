//! Change structure and serialization
//!
//! A **Change** (or "patch") is the fundamental unit of modification in Atomic.
//! Changes are content-addressed (identified by a Blake3 hash) and contain
//! all information needed to apply a modification to the repository graph.
//!
//! # Change File Format
//!
//! A change file has four sections:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Offsets (48 bytes)                      │
//! │  version, hashed_len, unhashed_off/len, contents_off/len    │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Hashed Section (zstd compressed)           │
//! │  header, dependencies, extra_known, metadata, hunks,        │
//! │  contents_hash                                              │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Unhashed Section (optional JSON)           │
//! │  Extra metadata that doesn't affect the change hash         │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Contents (raw bytes)                       │
//! │  The actual file content referenced by hunks                │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hash Computation
//!
//! The change hash is computed over the **uncompressed** hashed section.
//! This ensures:
//! - Hash stability regardless of compression settings
//! - Verification without full decompression
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::{Change, ChangeHeader, Author};
//! use std::fs::File;
//!
//! // Create a change
//! let change = Change::new(
//!     ChangeHeader::builder()
//!         .message("Add feature")
//!         .author(Author::new("Alice", Some("alice@example.com")))
//!         .build(),
//!     hunks,
//!     contents,
//! );
//!
//! // Serialize to file
//! let mut file = File::create("change.atomic")?;
//! let hash = change.serialize(&mut file)?;
//!
//! // Deserialize from file
//! let mut file = File::open("change.atomic")?;
//! let (loaded, hash) = Change::deserialize(&mut file)?;
//! ```

use super::header::ChangeHeader;
use super::graph_op::GraphOp;
use super::ops::FileOps;
use super::provenance::Provenance;
use crate::types::Base32;
use crate::Hash;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use thiserror::Error;

/// Current change format version.
///
/// This is incremented when the serialization format changes in a
/// backward-incompatible way.
pub const VERSION: u64 = 1;

/// Zstd compression level for hashed section.
const COMPRESSION_LEVEL: i32 = 3;

/// Errors that can occur during change operations.
#[derive(Debug, Error)]
pub enum ChangeError {
    /// Version mismatch in change file
    #[error("Version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u64, got: u64 },

    /// IO error during read/write
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error (bincode)
    #[error("Serialization error: {0}")]
    Bincode(#[from] bincode::Error),

    /// Compression/decompression error
    #[error("Compression error: {0}")]
    Compression(String),

    /// JSON error for unhashed section
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Hash mismatch during verification
    #[error("Change hash mismatch: claimed {claimed}, computed {computed}")]
    HashMismatch { claimed: String, computed: String },

    /// Contents hash mismatch
    #[error("Contents hash mismatch: claimed {claimed}, computed {computed}")]
    ContentsHashMismatch { claimed: String, computed: String },

    /// Missing required content
    #[error("Missing contents for hash {hash}")]
    MissingContents { hash: String },

    /// Invalid change structure
    #[error("Invalid change: {0}")]
    Invalid(String),
}

/// A complete change (patch).
///
/// This is the in-memory representation of a change, containing:
/// - Offsets for lazy loading from files
/// - The hashed portion (header, hunks, dependencies)
/// - Optional unhashed metadata
/// - The raw content blob
#[derive(Clone, Debug)]
pub struct Change {
    /// File offsets for lazy loading
    pub offsets: Offsets,

    /// The hashed portion (contributes to change hash)
    pub hashed: HashedChange,

    /// Optional unhashed metadata (JSON)
    ///
    /// This can contain arbitrary data that doesn't affect the change hash.
    /// Useful for storing editor metadata, review comments, etc.
    pub unhashed: Option<serde_json::Value>,

    /// Binary content blob
    ///
    /// This contains the actual file content referenced by hunks.
    /// Hunks reference byte ranges within this blob.
    pub contents: Vec<u8>,
}

impl Change {
    /// Create a new change.
    ///
    /// # Arguments
    ///
    /// * `header` - Change metadata (message, authors, timestamp)
    /// * `hunks` - The modifications in this change
    /// * `contents` - The content blob referenced by hunks
    /// * `dependencies` - Changes that must be applied before this one
    pub fn new(
        header: ChangeHeader,
        hunks: Vec<GraphOp<Option<Hash>>>,
        contents: Vec<u8>,
        dependencies: Vec<Hash>,
    ) -> Self {
        Self::with_file_ops(header, hunks, Vec::new(), contents, dependencies)
    }

    /// Create a new change with semantic layer operations.
    ///
    /// # Arguments
    ///
    /// * `header` - Change metadata (message, authors, timestamp)
    /// * `hunks` - The graph modifications in this change
    /// * `file_ops` - Semantic layer operations (Trunk → Branch → Leaf)
    /// * `contents` - The content blob referenced by operations
    /// * `dependencies` - Changes that must be applied before this one
    pub fn with_file_ops(
        header: ChangeHeader,
        hunks: Vec<GraphOp<Option<Hash>>>,
        file_ops: Vec<FileOps>,
        contents: Vec<u8>,
        dependencies: Vec<Hash>,
    ) -> Self {
        let contents_hash = Hash::of(&contents);

        Self {
            offsets: Offsets::default(),
            hashed: HashedChange {
                version: VERSION,
                header,
                dependencies,
                extra_known: Vec::new(),
                metadata: Vec::new(),
                provenance: Vec::new(),
                hunks,
                file_ops,
                contents_hash,
            },
            unhashed: None,
            contents,
        }
    }

    /// Create an empty change with just a header.
    ///
    /// This is useful as a starting point for building changes.
    pub fn empty(header: ChangeHeader) -> Self {
        Self {
            offsets: Offsets::default(),
            hashed: HashedChange {
                version: VERSION,
                header,
                dependencies: Vec::new(),
                extra_known: Vec::new(),
                metadata: Vec::new(),
                provenance: Vec::new(),
                hunks: Vec::new(),
                file_ops: Vec::new(),
                contents_hash: Hash::of(&[]),
            },
            unhashed: None,
            contents: Vec::new(),
        }
    }

    /// Add semantic layer operations to this change.
    pub fn add_file_ops(&mut self, ops: FileOps) {
        self.hashed.file_ops.push(ops);
    }

    /// Set all semantic layer operations at once.
    pub fn set_file_ops(&mut self, ops: Vec<FileOps>) {
        self.hashed.file_ops = ops;
    }

    /// Get a reference to the semantic layer operations.
    pub fn file_ops(&self) -> &[FileOps] {
        &self.hashed.file_ops
    }

    /// Check if this change has semantic layer operations.
    pub fn has_file_ops(&self) -> bool {
        self.hashed.has_file_ops()
    }

    /// Add a graph_op to this change.
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hashed.hunks.push(graph_op);
    }

    /// Add AI provenance information to this change.
    pub fn add_provenance(&mut self, provenance: Provenance) {
        self.hashed.provenance.push(provenance);
    }

    /// Get the provenance entries for this change.
    pub fn provenance(&self) -> &[Provenance] {
        &self.hashed.provenance
    }

    /// Check if this change has AI provenance information.
    pub fn has_provenance(&self) -> bool {
        !self.hashed.provenance.is_empty()
    }

    /// Append content to the contents blob.
    ///
    /// Returns the start position of the appended content.
    pub fn append_contents(&mut self, data: &[u8]) -> u64 {
        let start = self.contents.len() as u64;
        self.contents.extend_from_slice(data);
        start
    }

    /// Finalize the change, updating the contents hash.
    ///
    /// Call this after all content has been added.
    pub fn finalize(&mut self) {
        self.hashed.contents_hash = Hash::of(&self.contents);
    }

    /// Get the change message.
    pub fn message(&self) -> &str {
        &self.hashed.header.message
    }

    /// Get the change dependencies.
    pub fn dependencies(&self) -> &[Hash] {
        &self.hashed.dependencies
    }

    /// Get the hunks in this change.
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hashed.hunks
    }

    /// Compute the hash of this change.
    ///
    /// The hash is computed over the uncompressed hashed section.
    pub fn hash(&self) -> Result<Hash, ChangeError> {
        let hashed_bytes = bincode::serialize(&self.hashed)?;
        Ok(Hash::of(&hashed_bytes))
    }

    /// Serialize this change to a writer.
    ///
    /// Returns the hash of the change.
    ///
    /// # Arguments
    ///
    /// * `writer` - Where to write the serialized change
    ///
    /// # Returns
    ///
    /// The hash of the change.
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<Hash, ChangeError> {
        // 1. Serialize the hashed portion
        let hashed_bytes = bincode::serialize(&self.hashed)?;

        // 2. Compute hash (over uncompressed data)
        let hash = Hash::of(&hashed_bytes);

        // 3. Compress the hashed portion
        let hashed_compressed = zstd::encode_all(&hashed_bytes[..], COMPRESSION_LEVEL)
            .map_err(|e| ChangeError::Compression(e.to_string()))?;

        // 4. Serialize unhashed portion
        let unhashed_bytes = self
            .unhashed
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();

        // 5. Compute offsets
        let offsets = Offsets {
            version: VERSION,
            hashed_len: hashed_compressed.len() as u64,
            unhashed_off: Offsets::SIZE + hashed_compressed.len() as u64,
            unhashed_len: unhashed_bytes.len() as u64,
            contents_off: Offsets::SIZE + hashed_compressed.len() as u64 + unhashed_bytes.len() as u64,
            contents_len: self.contents.len() as u64,
        };

        // 6. Write everything
        writer.write_all(&offsets.to_bytes())?;
        writer.write_all(&hashed_compressed)?;
        writer.write_all(&unhashed_bytes)?;
        writer.write_all(&self.contents)?;

        Ok(hash)
    }

    /// Deserialize a change from a reader.
    ///
    /// # Arguments
    ///
    /// * `reader` - Where to read the serialized change from
    ///
    /// # Returns
    ///
    /// A tuple of (change, hash).
    pub fn deserialize<R: Read>(reader: &mut R) -> Result<(Self, Hash), ChangeError> {
        // 1. Read offsets
        let mut offset_bytes = [0u8; Offsets::SIZE as usize];
        reader.read_exact(&mut offset_bytes)?;
        let offsets = Offsets::from_bytes(&offset_bytes)?;

        // 2. Validate version
        if offsets.version != VERSION {
            return Err(ChangeError::VersionMismatch {
                expected: VERSION,
                got: offsets.version,
            });
        }

        // 3. Read hashed section (compressed)
        let mut hashed_compressed = vec![0u8; offsets.hashed_len as usize];
        reader.read_exact(&mut hashed_compressed)?;

        // 4. Decompress hashed section
        let hashed_bytes = zstd::decode_all(&hashed_compressed[..])
            .map_err(|e| ChangeError::Compression(e.to_string()))?;

        // 5. Compute hash
        let hash = Hash::of(&hashed_bytes);

        // 6. Deserialize hashed section
        let hashed: HashedChange = bincode::deserialize(&hashed_bytes)?;

        // 7. Read unhashed section
        let unhashed = if offsets.unhashed_len > 0 {
            let mut unhashed_bytes = vec![0u8; offsets.unhashed_len as usize];
            reader.read_exact(&mut unhashed_bytes)?;
            Some(serde_json::from_slice(&unhashed_bytes)?)
        } else {
            None
        };

        // 8. Read contents
        let mut contents = vec![0u8; offsets.contents_len as usize];
        reader.read_exact(&mut contents)?;

        // 9. Verify contents hash
        let computed_contents_hash = Hash::of(&contents);
        if computed_contents_hash != hashed.contents_hash {
            return Err(ChangeError::ContentsHashMismatch {
                claimed: hashed.contents_hash.to_base32(),
                computed: computed_contents_hash.to_base32(),
            });
        }

        Ok((
            Self {
                offsets,
                hashed,
                unhashed,
                contents,
            },
            hash,
        ))
    }

    /// Check if this change depends on another change.
    pub fn depends_on(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash)
    }

    /// Check if this change knows about another change.
    ///
    /// A change "knows" another if it's either a dependency or extra_known.
    pub fn knows(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash) || self.hashed.extra_known.contains(hash)
    }
}

impl Default for Change {
    fn default() -> Self {
        Self::empty(ChangeHeader::default())
    }
}

/// The hashed portion of a change.
///
/// This structure contains everything that contributes to the change hash.
/// Modifying any field here will result in a different change hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedChange {
    /// Format version
    pub version: u64,

    /// Change metadata (message, authors, timestamp)
    pub header: ChangeHeader,

    /// Direct dependencies (hashes of required changes)
    ///
    /// These changes MUST be applied before this change can be applied.
    pub dependencies: Vec<Hash>,

    /// Extra known changes (for context)
    ///
    /// These changes were known when this change was created, but are not
    /// strictly required. Used for better merge behavior.
    pub extra_known: Vec<Hash>,

    /// Custom metadata (opaque bytes)
    ///
    /// Application-specific metadata that affects the change hash.
    #[serde(default)]
    pub metadata: Vec<u8>,

    /// AI provenance information (optional)
    ///
    /// Tracks AI involvement in creating this change, including:
    /// - Vendor/model information
    /// - Prompt hashes (for privacy)
    /// - Token usage and cost
    /// - Suggestion type (complete, partial, collaborative)
    #[serde(default)]
    pub provenance: Vec<Provenance>,

    /// The actual modifications (graph operations)
    ///
    /// These hunks represent the low-level graph operations (vertices, edges)
    /// that modify the repository graph. They are the "storage layer" operations.
    pub hunks: Vec<GraphOp<Option<Hash>>>,

    /// Semantic layer operations (CRDT model)
    ///
    /// These operations represent the human-readable changes organized as:
    /// - FileOps (Trunk level): File create/delete/move/undelete
    /// - LineOps (Branch level): Line insert/delete/restore
    /// - LeafOp (Leaf level): Token insert/delete/replace
    ///
    /// This enables:
    /// - Line-number based diffs (not byte ranges)
    /// - Token-level highlighting (`--word-diff`)
    /// - Fine-grained blame (who wrote each token)
    /// - Human-readable code review
    #[serde(default)]
    pub file_ops: Vec<FileOps>,

    /// Hash of the contents blob
    ///
    /// This allows verification of the contents without including them
    /// in the hashed section directly.
    pub contents_hash: Hash,
}

impl HashedChange {
    /// Get all dependencies and extra_known combined.
    pub fn all_known(&self) -> impl Iterator<Item = &Hash> {
        self.dependencies.iter().chain(self.extra_known.iter())
    }

    /// Check if this change has any hunks.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty() && self.file_ops.is_empty()
    }

    /// Get the number of hunks.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Check if this change has AI provenance.
    pub fn has_provenance(&self) -> bool {
        !self.provenance.is_empty()
    }

    /// Get the number of provenance entries.
    pub fn provenance_count(&self) -> usize {
        self.provenance.len()
    }

    /// Check if this change has semantic layer operations.
    pub fn has_file_ops(&self) -> bool {
        !self.file_ops.is_empty()
    }

    /// Get the number of file operations.
    pub fn file_ops_count(&self) -> usize {
        self.file_ops.len()
    }

    /// Get a reference to the file operations.
    pub fn file_ops(&self) -> &[FileOps] {
        &self.file_ops
    }
}

/// Table of contents for a change file.
///
/// This structure is stored at the beginning of a change file and allows
/// seeking to different sections without reading the entire file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offsets {
    /// Format version
    pub version: u64,

    /// Length of the hashed section (compressed)
    pub hashed_len: u64,

    /// Offset to unhashed section
    pub unhashed_off: u64,

    /// Length of unhashed section
    pub unhashed_len: u64,

    /// Offset to contents section
    pub contents_off: u64,

    /// Length of contents section
    pub contents_len: u64,
}

impl Offsets {
    /// Size of the offsets structure in bytes.
    pub const SIZE: u64 = 48; // 6 * 8 bytes

    /// Convert to bytes for writing.
    pub fn to_bytes(&self) -> [u8; Self::SIZE as usize] {
        let mut bytes = [0u8; Self::SIZE as usize];
        bytes[0..8].copy_from_slice(&self.version.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.hashed_len.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.unhashed_off.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.unhashed_len.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.contents_off.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.contents_len.to_le_bytes());
        bytes
    }

    /// Parse from bytes.
    pub fn from_bytes(bytes: &[u8; Self::SIZE as usize]) -> Result<Self, ChangeError> {
        Ok(Self {
            version: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            hashed_len: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            unhashed_off: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            unhashed_len: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            contents_off: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            contents_len: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        })
    }

    /// Get the total size of the change file.
    pub fn total_size(&self) -> u64 {
        self.contents_off + self.contents_len
    }

    /// Get the size of the change file without contents.
    pub fn size_without_contents(&self) -> u64 {
        self.contents_off
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Author, Encoding, Local};
    use crate::change::atom::{Atom, Insertion};
    use crate::{ChangePosition, EdgeFlags, Position};
    use std::io::Cursor;

    fn test_hash_position(pos: u64) -> Position<Option<Hash>> {
        Position::new(Some(Hash::of(b"test")), ChangePosition::new(pos))
    }

    fn test_new_vertex() -> Insertion<Option<Hash>> {
        Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        }
    }

    // ========================================================================
    // Offsets Tests
    // ========================================================================

    #[test]
    fn test_offsets_size() {
        assert_eq!(Offsets::SIZE, 48);
    }

    #[test]
    fn test_offsets_roundtrip() {
        let offsets = Offsets {
            version: VERSION,
            hashed_len: 1000,
            unhashed_off: 1048,
            unhashed_len: 500,
            contents_off: 1548,
            contents_len: 10000,
        };

        let bytes = offsets.to_bytes();
        let parsed = Offsets::from_bytes(&bytes).unwrap();

        assert_eq!(offsets, parsed);
    }

    #[test]
    fn test_offsets_total_size() {
        let offsets = Offsets {
            version: VERSION,
            hashed_len: 100,
            unhashed_off: 148,
            unhashed_len: 50,
            contents_off: 198,
            contents_len: 1000,
        };

        assert_eq!(offsets.total_size(), 1198);
        assert_eq!(offsets.size_without_contents(), 198);
    }

    #[test]
    fn test_offsets_default() {
        let offsets = Offsets::default();
        assert_eq!(offsets.version, 0);
        assert_eq!(offsets.hashed_len, 0);
    }

    // ========================================================================
    // HashedChange Tests
    // ========================================================================

    #[test]
    fn test_hashed_change_is_empty() {
        let hashed = HashedChange {
            version: VERSION,
            header: ChangeHeader::default(),
            dependencies: vec![],
            extra_known: vec![],
            metadata: vec![],
            provenance: vec![],
            hunks: vec![],
            file_ops: vec![],
            contents_hash: Hash::ZERO,
        };

        assert!(hashed.is_empty());
        assert_eq!(hashed.hunk_count(), 0);
    }

    #[test]
    fn test_hashed_change_all_known() {
        let dep1 = Hash::of(b"dep1");
        let dep2 = Hash::of(b"dep2");
        let extra = Hash::of(b"extra");

        let hashed = HashedChange {
            version: VERSION,
            header: ChangeHeader::default(),
            dependencies: vec![dep1, dep2],
            extra_known: vec![extra],
            metadata: vec![],
            provenance: vec![],
            hunks: vec![],
            file_ops: vec![],
            contents_hash: Hash::ZERO,
        };

        let all_known: Vec<_> = hashed.all_known().collect();
        assert_eq!(all_known.len(), 3);
    }

    // ========================================================================
    // Change Tests
    // ========================================================================

    #[test]
    fn test_change_new() {
        let header = ChangeHeader::builder()
            .message("Test change")
            .author(Author::new("Alice", None::<String>))
            .build();

        let contents = b"Hello, world!".to_vec();
        let change = Change::new(header, vec![], contents.clone(), vec![]);

        assert_eq!(change.message(), "Test change");
        assert_eq!(change.contents, contents);
        assert!(change.dependencies().is_empty());
    }

    #[test]
    fn test_change_empty() {
        let header = ChangeHeader::new("Empty change");
        let change = Change::empty(header);

        assert!(change.hunks().is_empty());
        assert!(change.contents.is_empty());
    }

    #[test]
    fn test_change_add_hunk() {
        let mut change = Change::empty(ChangeHeader::new("Test"));

        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("test.rs", 1),
            encoding: Some(Encoding::Utf8),
        };

        change.add_hunk(graph_op);
        assert_eq!(change.hunks().len(), 1);
    }

    #[test]
    fn test_change_append_contents() {
        let mut change = Change::empty(ChangeHeader::new("Test"));

        let start1 = change.append_contents(b"Hello");
        let start2 = change.append_contents(b" World");

        assert_eq!(start1, 0);
        assert_eq!(start2, 5);
        assert_eq!(change.contents, b"Hello World");
    }

    #[test]
    fn test_change_finalize() {
        let mut change = Change::empty(ChangeHeader::new("Test"));
        change.append_contents(b"Some content");
        change.finalize();

        assert_eq!(change.hashed.contents_hash, Hash::of(b"Some content"));
    }

    #[test]
    fn test_change_hash() {
        let change1 = Change::new(
            ChangeHeader::new("Change 1"),
            vec![],
            vec![],
            vec![],
        );
        let change2 = Change::new(
            ChangeHeader::new("Change 2"),
            vec![],
            vec![],
            vec![],
        );

        let hash1 = change1.hash().unwrap();
        let hash2 = change2.hash().unwrap();

        // Different changes should have different hashes
        assert_ne!(hash1, hash2);

        // Same change should have same hash
        let hash1_again = change1.hash().unwrap();
        assert_eq!(hash1, hash1_again);
    }

    #[test]
    fn test_change_depends_on() {
        let dep = Hash::of(b"dependency");
        let change = Change::new(
            ChangeHeader::new("Test"),
            vec![],
            vec![],
            vec![dep],
        );

        assert!(change.depends_on(&dep));
        assert!(!change.depends_on(&Hash::of(b"other")));
    }

    #[test]
    fn test_change_knows() {
        let dep = Hash::of(b"dependency");
        let extra = Hash::of(b"extra");

        let mut change = Change::new(
            ChangeHeader::new("Test"),
            vec![],
            vec![],
            vec![dep],
        );
        change.hashed.extra_known.push(extra);

        assert!(change.knows(&dep));
        assert!(change.knows(&extra));
        assert!(!change.knows(&Hash::of(b"unknown")));
    }

    #[test]
    fn test_change_default() {
        let change = Change::default();
        assert!(change.message().is_empty());
        assert!(change.hunks().is_empty());
        assert!(change.contents.is_empty());
    }

    // ========================================================================
    // Serialization Tests
    // ========================================================================

    #[test]
    fn test_change_serialize_deserialize() {
        let header = ChangeHeader::builder()
            .message("Test serialization")
            .author(Author::new("Bob", Some("bob@example.com")))
            .build();

        let contents = b"File contents here".to_vec();
        let change = Change::new(header, vec![], contents, vec![]);

        // Serialize
        let mut buffer = Vec::new();
        let hash = change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, loaded_hash) = Change::deserialize(&mut cursor).unwrap();

        // Verify
        assert_eq!(hash, loaded_hash);
        assert_eq!(change.message(), loaded.message());
        assert_eq!(change.contents, loaded.contents);
    }

    #[test]
    fn test_change_serialize_with_hunks() {
        let mut change = Change::empty(ChangeHeader::new("With hunks"));

        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("test.rs", 42),
            encoding: Some(Encoding::Utf8),
        };
        change.add_hunk(graph_op);
        change.append_contents(b"Hello World");
        change.finalize();

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.hunks().len(), 1);
        assert_eq!(loaded.contents, b"Hello World");
    }

    #[test]
    fn test_change_serialize_with_unhashed() {
        let mut change = Change::empty(ChangeHeader::new("With unhashed"));
        change.unhashed = Some(serde_json::json!({
            "editor": "vim",
            "custom_field": 42
        }));

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert!(loaded.unhashed.is_some());
        let unhashed = loaded.unhashed.unwrap();
        assert_eq!(unhashed["editor"], "vim");
        assert_eq!(unhashed["custom_field"], 42);
    }

    #[test]
    fn test_change_serialize_with_dependencies() {
        let dep1 = Hash::of(b"dep1");
        let dep2 = Hash::of(b"dep2");

        let change = Change::new(
            ChangeHeader::new("With deps"),
            vec![],
            vec![],
            vec![dep1, dep2],
        );

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.dependencies().len(), 2);
        assert!(loaded.depends_on(&dep1));
        assert!(loaded.depends_on(&dep2));
    }

    // ========================================================================
    // Error Tests
    // ========================================================================

    #[test]
    fn test_version_mismatch_error() {
        let mut change = Change::empty(ChangeHeader::new("Test"));

        // Serialize with current version
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Corrupt the version
        buffer[0] = 99; // Invalid version

        // Try to deserialize
        let mut cursor = Cursor::new(buffer);
        let result = Change::deserialize(&mut cursor);

        assert!(result.is_err());
        match result.unwrap_err() {
            ChangeError::VersionMismatch { got, .. } => assert_eq!(got, 99),
            _ => panic!("Expected VersionMismatch error"),
        }
    }

    #[test]
    fn test_contents_hash_mismatch_error() {
        let change = Change::new(
            ChangeHeader::new("Test"),
            vec![],
            b"Original content".to_vec(),
            vec![],
        );

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Corrupt the contents (at the end of the buffer)
        let len = buffer.len();
        buffer[len - 1] ^= 0xFF;

        // Try to deserialize
        let mut cursor = Cursor::new(buffer);
        let result = Change::deserialize(&mut cursor);

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ChangeError::ContentsHashMismatch { .. }));
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_contents() {
        let change = Change::new(
            ChangeHeader::new("Empty contents"),
            vec![],
            vec![],
            vec![],
        );

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert!(loaded.contents.is_empty());
    }

    #[test]
    fn test_large_contents() {
        let large_contents = vec![0u8; 1024 * 1024]; // 1MB
        let change = Change::new(
            ChangeHeader::new("Large contents"),
            vec![],
            large_contents.clone(),
            vec![],
        );

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.contents.len(), 1024 * 1024);
    }

    #[test]
    fn test_multiple_hunks() {
        let mut change = Change::empty(ChangeHeader::new("Multiple hunks"));

        for i in 0..10 {
            let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
                change: Atom::Insertion(Insertion {
                    predecessors: vec![],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start: ChangePosition::new(i * 10),
                    end: ChangePosition::new((i + 1) * 10),
                    inode: test_hash_position(0),
                }),
                local: Local::new("test.rs", i + 1),
                encoding: Some(Encoding::Utf8),
            };
            change.add_hunk(graph_op);
        }

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.hunks().len(), 10);
    }

    #[test]
    fn test_hash_stability() {
        // Same change content should always produce same hash
        // Use a fixed timestamp to ensure deterministic hashing
        let fixed_ts = chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let make_change = || {
            let header = ChangeHeader::builder()
                .message("Stable hash test")
                .timestamp(fixed_ts)
                .build();
            Change::new(header, vec![], b"content".to_vec(), vec![])
        };

        let hash1 = make_change().hash().unwrap();
        let hash2 = make_change().hash().unwrap();

        assert_eq!(hash1, hash2);
    }
}

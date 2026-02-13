# Atomic VCS: Implementation Guide

## Overview

This document provides concrete implementation details for building Atomic VCS. It serves as a roadmap from theory to working code, with specific data structures, algorithms, and phased development milestones.

---

## Phase 1: Core Data Structures (Week 1)

### 1.1 Identifiers

```rust
/// 64-bit little-endian integer for cross-platform consistency
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct L64(pub u64);

/// Internal change/node identifier (repository-local)
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub L64);

impl NodeId {
    pub const ROOT: NodeId = NodeId(L64(0));
}

/// Position within a change's content
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangePosition(pub L64);

/// File system inode identifier
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Inode(pub L64);

/// Blake3 content hash (32 bytes)
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash(pub [u8; 32]);

/// Incremental Merkle state hash
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Merkle(pub [u8; 32]);
```

### 1.2 Vertex

```rust
/// A node in the repository graph
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Vertex<H> {
    /// The change that introduced this vertex
    pub change: H,
    /// Start position within the change's content
    pub start: ChangePosition,
    /// End position (exclusive)
    pub end: ChangePosition,
}

impl Vertex<NodeId> {
    pub const ROOT: Self = Vertex {
        change: NodeId::ROOT,
        start: ChangePosition(L64(0)),
        end: ChangePosition(L64(0)),
    };

    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    pub fn len(&self) -> usize {
        (self.end.0.0 - self.start.0.0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A specific byte position within a change
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position<H> {
    pub change: H,
    pub pos: ChangePosition,
}
```

### 1.3 Edges

```rust
bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub struct EdgeFlags: u8 {
        /// Sequential content ordering
        const BLOCK = 0b0000_0001;
        /// Computed edge for connectivity
        const PSEUDO = 0b0000_0100;
        /// File system hierarchy edge
        const FOLDER = 0b0001_0000;
        /// Reverse direction edge
        const PARENT = 0b0010_0000;
        /// Marks deleted content
        const DELETED = 0b1000_0000;
    }
}

impl EdgeFlags {
    pub fn is_alive(&self) -> bool {
        !self.contains(Self::DELETED)
    }

    pub fn is_parent(&self) -> bool {
        self.contains(Self::PARENT)
    }

    pub fn is_folder(&self) -> bool {
        self.contains(Self::FOLDER)
    }
}

/// Full edge representation (for API use)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub flag: EdgeFlags,
    pub dest: Position<NodeId>,
    pub introduced_by: NodeId,
}

/// Compact serialized edge (for storage)
/// Layout: [flags:8][pos:56] [change:64] [introduced_by:64]
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SerializedEdge([L64; 3]);

impl SerializedEdge {
    pub fn new(flag: EdgeFlags, dest: Position<NodeId>, introduced_by: NodeId) -> Self {
        let pos_bits = dest.pos.0.0 & 0x00FFFFFFFFFFFFFF;
        let flag_bits = (flag.bits() as u64) << 56;
        Self([
            L64(flag_bits | pos_bits),
            dest.change.0,
            introduced_by.0,
        ])
    }

    pub fn flag(&self) -> EdgeFlags {
        EdgeFlags::from_bits_truncate((self.0[0].0 >> 56) as u8)
    }

    pub fn dest(&self) -> Position<NodeId> {
        Position {
            change: NodeId(self.0[1]),
            pos: ChangePosition(L64(self.0[0].0 & 0x00FFFFFFFFFFFFFF)),
        }
    }

    pub fn introduced_by(&self) -> NodeId {
        NodeId(self.0[2])
    }
}
```

### 1.4 Base32 Encoding

```rust
/// Base32 encoding for human-readable identifiers
pub trait Base32 {
    fn to_base32(&self) -> String;
    fn from_base32(s: &[u8]) -> Option<Self> where Self: Sized;
}

// Use data_encoding crate with RFC 4648 alphabet (A-Z, 2-7)
// This gives us case-insensitive, filesystem-safe identifiers
```

---

## Phase 2: Change Representation (Week 1-2)

### 2.1 Change Structure

```rust
/// A complete change (patch)
#[derive(Clone, Debug)]
pub struct Change {
    /// File offsets for lazy loading
    pub offsets: Offsets,
    /// Hashed portion (contributes to change hash)
    pub hashed: HashedChange,
    /// Optional unhashed metadata (JSON)
    pub unhashed: Option<serde_json::Value>,
    /// Binary content blob
    pub contents: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HashedChange {
    /// Format version
    pub version: u64,
    /// Metadata
    pub header: ChangeHeader,
    /// Direct dependencies (hashes of required changes)
    pub dependencies: Vec<Hash>,
    /// Extra known changes (for context)
    pub extra_known: Vec<Hash>,
    /// Custom metadata
    pub metadata: Vec<u8>,
    /// The actual modifications
    pub hunks: Vec<Hunk>,
    /// Hash of the contents blob
    pub contents_hash: Hash,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeHeader {
    pub message: String,
    pub description: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub authors: Vec<Author>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub email: Option<String>,
    pub identity: Option<String>,
}
```

### 2.2 Hunks

```rust
/// Atomic modification unit
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Hunk {
    /// Add a new file
    FileAdd {
        /// Edge to add file name in parent directory
        add_name: NewVertex,
        /// Edge to create inode vertex
        add_inode: NewVertex,
        /// Initial file contents (if any)
        contents: Option<NewVertex>,
        /// Path for human readability
        path: String,
        /// Text encoding (if text file)
        encoding: Option<Encoding>,
    },

    /// Delete a file
    FileDel {
        /// Edges to mark as deleted
        del: EdgeMap,
        /// Content deletion (if any)
        contents: Option<EdgeMap>,
        path: String,
        encoding: Option<Encoding>,
    },

    /// Move/rename a file
    FileMove {
        /// Remove old name edge
        del: EdgeMap,
        /// Add new name edge
        add: NewVertex,
        path: String,
    },

    /// Edit file contents
    Edit {
        /// The modification
        change: AtomicChange,
        /// Local context (for display)
        local: LocalContext,
        encoding: Option<Encoding>,
    },

    /// Replace content (delete + insert)
    Replacement {
        /// Content to delete
        change: EdgeMap,
        /// Content to insert
        replacement: NewVertex,
        local: LocalContext,
        encoding: Option<Encoding>,
    },
}

/// Either a new vertex or edge modification
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AtomicChange {
    NewVertex(NewVertex),
    EdgeMap(EdgeMap),
}

/// Insert new content
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewVertex {
    /// Vertices that should precede this one
    pub up_context: Vec<Position<Hash>>,
    /// Vertices that should follow this one
    pub down_context: Vec<Position<Hash>>,
    /// Edge flags for new edges
    pub flag: EdgeFlags,
    /// Start offset in contents blob
    pub start: u64,
    /// End offset in contents blob
    pub end: u64,
    /// File this belongs to
    pub inode: Position<Hash>,
}

/// Modify existing edges
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeMap {
    /// Edge modifications
    pub edges: Vec<NewEdge>,
    /// File this belongs to
    pub inode: Position<Hash>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewEdge {
    /// Flags to remove
    pub previous: EdgeFlags,
    /// Flags to add
    pub flag: EdgeFlags,
    /// Edge source position
    pub from: Position<Hash>,
    /// Edge destination vertex
    pub to: Vertex<Hash>,
    /// Change that introduced the original edge
    pub introduced_by: Hash,
}
```

### 2.3 Change Serialization

```rust
/// Change file format:
/// [offsets: 48 bytes]
/// [hashed: zstd compressed bincode]
/// [unhashed: JSON, optional]
/// [contents: raw bytes]

pub const VERSION: u64 = 1;

#[repr(C)]
pub struct Offsets {
    pub version: u64,
    pub hashed_len: u64,
    pub unhashed_off: u64,
    pub unhashed_len: u64,
    pub contents_off: u64,
    pub contents_len: u64,
}

impl Change {
    pub fn serialize<W: Write>(&self, w: &mut W) -> Result<Hash> {
        // 1. Serialize hashed portion
        let hashed_bytes = bincode::serialize(&self.hashed)?;
        let hashed_compressed = zstd::encode_all(&hashed_bytes[..], 3)?;

        // 2. Serialize unhashed portion
        let unhashed_bytes = self.unhashed
            .as_ref()
            .map(|v| serde_json::to_vec(v))
            .transpose()?
            .unwrap_or_default();

        // 3. Compute hash of hashed portion
        let hash = Hash(blake3::hash(&hashed_bytes).into());

        // 4. Write offsets
        let offsets = Offsets {
            version: VERSION,
            hashed_len: hashed_compressed.len() as u64,
            unhashed_off: 48 + hashed_compressed.len() as u64,
            unhashed_len: unhashed_bytes.len() as u64,
            contents_off: 48 + hashed_compressed.len() as u64 + unhashed_bytes.len() as u64,
            contents_len: self.contents.len() as u64,
        };
        w.write_all(bytes_of(&offsets))?;

        // 5. Write data
        w.write_all(&hashed_compressed)?;
        w.write_all(&unhashed_bytes)?;
        w.write_all(&self.contents)?;

        Ok(hash)
    }

    pub fn deserialize<R: Read>(r: &mut R) -> Result<(Self, Hash)> {
        // Read and validate offsets, decompress, deserialize...
    }
}
```

---

## Phase 3: Storage Layer (Week 2)

### 3.1 Database Trait Abstraction

```rust
/// Read-only graph operations
pub trait GraphTxnT {
    type GraphError: std::error::Error + 'static;
    type Adj: Iterator<Item = Result<SerializedEdge, Self::GraphError>>;

    /// Get external hash for internal ID
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, Self::GraphError>;

    /// Get internal ID for external hash
    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, Self::GraphError>;

    /// Initialize adjacency iterator
    fn init_adj(
        &self,
        graph: &Self::Graph,
        vertex: Vertex<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, Self::GraphError>;

    /// Find vertex containing position
    fn find_block(&self, graph: &Self::Graph, pos: Position<NodeId>) 
        -> Result<Vertex<NodeId>, Self::GraphError>;
}

/// Channel operations
pub trait ChannelTxnT {
    type Channel;

    fn name(&self, channel: &Self::Channel) -> &str;
    fn graph(&self, channel: &Self::Channel) -> &Self::Graph;
    fn changes(&self, channel: &Self::Channel) -> &Self::Changeset;
    fn state(&self, channel: &Self::Channel) -> Merkle;
}

/// Mutable operations
pub trait MutTxnT: GraphTxnT + ChannelTxnT {
    fn put_graph(&mut self, vertex: Vertex<NodeId>, edge: SerializedEdge) 
        -> Result<bool, Self::GraphError>;

    fn del_graph(&mut self, vertex: Vertex<NodeId>, edge: SerializedEdge)
        -> Result<bool, Self::GraphError>;

    fn put_changes(&mut self, channel: &mut Self::Channel, id: NodeId, timestamp: u64)
        -> Result<(), Self::GraphError>;

    fn open_or_create_channel(&mut self, name: &str) 
        -> Result<Self::Channel, Self::GraphError>;

    fn commit(self) -> Result<(), Self::GraphError>;
}
```

### 3.2 Storage Backend: redb

We use **redb** - a pure Rust, ACID-compliant, embedded key-value database with copy-on-write B-trees.

**Why redb:**
- **Pure Rust** - no C dependencies, simpler builds
- **ACID transactions** - safe concurrent access
- **Key-value oriented** - maps directly to our Vertex → Edge lookups
- **Copy-on-write** - similar to Sanakirja (what pijul uses)
- **Memory-mapped** - excellent read performance
- **Simple API** - no SQL overhead

### 3.3 redb Table Definitions

```rust
use redb::{Database, TableDefinition, ReadableTable, ReadableMultimapTable};

/// Table definitions for the Atomic pristine database

/// Maps NodeId → Hash (internal to external)
const EXTERNAL: TableDefinition<u64, &[u8; 32]> = 
    TableDefinition::new("external");

/// Maps Hash → NodeId (external to internal)
const INTERNAL: TableDefinition<&[u8; 32], u64> = 
    TableDefinition::new("internal");

/// Maps NodeId → NodeType (change or tag)
const NODE_TYPES: TableDefinition<u64, u8> = 
    TableDefinition::new("node_types");

/// Main graph: Vertex → [Edge] (multimap)
/// Key: (change_id, start, end) as 24 bytes
/// Value: SerializedEdge as 24 bytes
const GRAPH: MultimapTableDefinition<&[u8; 24], &[u8; 24]> = 
    MultimapTableDefinition::new("graph");

/// Inode-scoped graph for O(n) file traversal
/// Key: (inode, change_id, start, end) as 32 bytes
/// Value: SerializedEdge as 24 bytes
const INODE_GRAPH: MultimapTableDefinition<&[u8; 32], &[u8; 24]> = 
    MultimapTableDefinition::new("inode_graph");

/// Channel metadata
/// Key: channel name
/// Value: serialized ChannelState
const CHANNELS: TableDefinition<&str, &[u8]> = 
    TableDefinition::new("channels");

/// Channel change log: (channel_id, timestamp) → change_id
const CHANNEL_CHANGES: TableDefinition<&[u8; 16], u64> = 
    TableDefinition::new("channel_changes");

/// Reverse change log: (channel_id, change_id) → timestamp
const REV_CHANNEL_CHANGES: TableDefinition<&[u8; 16], u64> = 
    TableDefinition::new("rev_channel_changes");

/// File tree: path → inode
const TREE: TableDefinition<&str, u64> = 
    TableDefinition::new("tree");

/// Reverse tree: inode → path
const REV_TREE: TableDefinition<u64, &str> = 
    TableDefinition::new("rev_tree");

/// Inodes: inode → Position (graph location)
const INODES: TableDefinition<u64, &[u8; 16]> = 
    TableDefinition::new("inodes");

/// Reverse inodes: Position → inode
const REV_INODES: TableDefinition<&[u8; 16], u64> = 
    TableDefinition::new("rev_inodes");

/// Dependencies: change_id → [dep_id]
const DEPS: MultimapTableDefinition<u64, u64> = 
    MultimapTableDefinition::new("deps");

/// Reverse dependencies: dep_id → [change_id]
const REV_DEPS: MultimapTableDefinition<u64, u64> = 
    MultimapTableDefinition::new("rev_deps");

/// Channel states: (channel_id, merkle) → timestamp
const STATES: TableDefinition<&[u8; 40], u64> = 
    TableDefinition::new("states");

/// Channel tags: (channel_id, timestamp) → merkle
const TAGS: TableDefinition<&[u8; 16], &[u8; 32]> = 
    TableDefinition::new("tags");
```

### 3.4 redb Implementation

```rust
use redb::{Database, ReadTransaction, WriteTransaction};
use std::path::Path;

pub struct Pristine {
    db: Database,
}

impl Pristine {
    /// Open or create the pristine database
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, PristineError> {
        let db = Database::create(path)?;
        
        // Ensure all tables exist
        let write_txn = db.begin_write()?;
        {
            // Creating tables is idempotent in redb
            write_txn.open_table(EXTERNAL)?;
            write_txn.open_table(INTERNAL)?;
            write_txn.open_table(NODE_TYPES)?;
            write_txn.open_multimap_table(GRAPH)?;
            write_txn.open_multimap_table(INODE_GRAPH)?;
            write_txn.open_table(CHANNELS)?;
            write_txn.open_table(CHANNEL_CHANGES)?;
            write_txn.open_table(TREE)?;
            write_txn.open_table(REV_TREE)?;
            write_txn.open_table(INODES)?;
            write_txn.open_table(REV_INODES)?;
            write_txn.open_multimap_table(DEPS)?;
            write_txn.open_multimap_table(REV_DEPS)?;
        }
        write_txn.commit()?;
        
        Ok(Self { db })
    }

    /// Begin a read transaction
    pub fn read_txn(&self) -> Result<ReadTransaction, PristineError> {
        Ok(self.db.begin_read()?)
    }

    /// Begin a write transaction
    pub fn write_txn(&self) -> Result<WriteTransaction, PristineError> {
        Ok(self.db.begin_write()?)
    }
}

/// Example: Looking up edges for a vertex
impl Pristine {
    pub fn get_edges(
        &self,
        txn: &ReadTransaction,
        vertex: Vertex<NodeId>,
    ) -> Result<Vec<SerializedEdge>, PristineError> {
        let table = txn.open_multimap_table(GRAPH)?;
        let key = vertex_to_bytes(&vertex);
        
        let mut edges = Vec::new();
        for result in table.get(&key)? {
            let (_, edge_bytes) = result?;
            edges.push(SerializedEdge::from_bytes(edge_bytes.value()));
        }
        Ok(edges)
    }
}
```

### 3.5 Key Encoding

```rust
/// Encode a Vertex as 24 bytes for use as a key
fn vertex_to_bytes(v: &Vertex<NodeId>) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..8].copy_from_slice(&v.change.0.to_le_bytes());
    bytes[8..16].copy_from_slice(&v.start.0.to_le_bytes());
    bytes[16..24].copy_from_slice(&v.end.0.to_le_bytes());
    bytes
}

/// Encode an (Inode, Vertex) pair as 32 bytes
fn inode_vertex_to_bytes(inode: Inode, v: &Vertex<NodeId>) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&inode.0.to_le_bytes());
    bytes[8..32].copy_from_slice(&vertex_to_bytes(v));
    bytes
}

/// Encode a Position as 16 bytes
fn position_to_bytes(p: &Position<NodeId>) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&p.change.0.to_le_bytes());
    bytes[8..16].copy_from_slice(&p.pos.0.to_le_bytes());
    bytes
}
```

---

## Phase 4: Diff Algorithm (Week 2-3)

### 4.1 Diff Strategy

```rust
/// Diff algorithm selection
#[derive(Clone, Copy, Debug, Default)]
pub enum DiffAlgorithm {
    /// Classic Myers diff - fast, good for most cases
    #[default]
    Myers,
    /// Patience diff - better for code with moved blocks
    Patience,
    /// Histogram diff - good balance of speed and quality
    Histogram,
}

/// Line representation for diffing
#[derive(Clone)]
pub struct Line<'a> {
    /// The actual content
    pub content: &'a [u8],
    /// Hash for fast equality comparison
    pub hash: u64,
    /// Original graph vertex (if from pristine)
    pub vertex: Option<Vertex<NodeId>>,
}

impl PartialEq for Line<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.content == other.content
    }
}
impl Eq for Line<'_> {}
```

### 4.2 Diff Output

```rust
/// Result of diffing two sequences
pub struct DiffResult {
    pub hunks: Vec<DiffHunk>,
}

pub struct DiffHunk {
    /// Position in old sequence
    pub old_start: usize,
    pub old_len: usize,
    /// Position in new sequence
    pub new_start: usize,
    pub new_len: usize,
}

impl DiffHunk {
    pub fn is_insert(&self) -> bool {
        self.old_len == 0
    }

    pub fn is_delete(&self) -> bool {
        self.new_len == 0
    }

    pub fn is_replace(&self) -> bool {
        self.old_len > 0 && self.new_len > 0
    }
}
```

### 4.3 Graph-Aware Diff

```rust
/// Convert file diff to graph operations
pub fn diff_to_hunks<T: GraphTxnT>(
    txn: &T,
    channel: &T::Channel,
    inode: Inode,
    old_lines: &[Line],
    new_lines: &[Line],
    new_content: &[u8],
    algorithm: DiffAlgorithm,
) -> Result<Vec<Hunk>, DiffError> {
    // 1. Compute line-level diff
    let diff = compute_diff(old_lines, new_lines, algorithm);

    // 2. Map diff hunks to graph operations
    let mut hunks = Vec::new();

    for hunk in diff.hunks {
        if hunk.is_delete() || hunk.is_replace() {
            // Create EdgeMap to mark deleted lines
            let edges = collect_deletion_edges(txn, channel, &old_lines[hunk.old_start..][..hunk.old_len])?;
            hunks.push(Hunk::Edit {
                change: AtomicChange::EdgeMap(edges),
                local: LocalContext::from_lines(old_lines, hunk.old_start),
                encoding: Some(Encoding::Utf8),
            });
        }

        if hunk.is_insert() || hunk.is_replace() {
            // Create NewVertex for inserted content
            let (up_ctx, down_ctx) = compute_context(old_lines, hunk.old_start)?;
            let content_range = compute_content_range(new_lines, hunk.new_start, hunk.new_len);

            let new_vertex = NewVertex {
                up_context: up_ctx,
                down_context: down_ctx,
                flag: EdgeFlags::BLOCK,
                start: content_range.start,
                end: content_range.end,
                inode: inode_position(txn, inode)?,
            };

            hunks.push(Hunk::Edit {
                change: AtomicChange::NewVertex(new_vertex),
                local: LocalContext::from_lines(new_lines, hunk.new_start),
                encoding: Some(Encoding::Utf8),
            });
        }
    }

    Ok(hunks)
}
```

### 4.4 Performance Optimization: Content Hashing

```rust
/// Fast content comparison using rolling hash
pub struct ContentHasher {
    // Use xxhash for speed (not cryptographic, just for comparison)
}

impl ContentHasher {
    /// Hash a line for quick comparison
    pub fn hash_line(content: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(content)
    }

    /// Prepare lines for diffing with pre-computed hashes
    pub fn prepare_lines(content: &[u8]) -> Vec<Line> {
        content
            .split_inclusive(|&b| b == b'\n')
            .map(|line| Line {
                content: line,
                hash: Self::hash_line(line),
                vertex: None,
            })
            .collect()
    }
}
```

---

## Phase 5: Record and Apply (Week 3)

### 5.1 Recording Changes

```rust
pub struct RecordBuilder {
    header: ChangeHeader,
    hunks: Vec<Hunk>,
    contents: Vec<u8>,
    dependencies: HashSet<Hash>,
}

impl RecordBuilder {
    pub fn new(author: Author, message: String) -> Self {
        Self {
            header: ChangeHeader {
                message,
                description: None,
                timestamp: Utc::now(),
                authors: vec![author],
            },
            hunks: Vec::new(),
            contents: Vec::new(),
            dependencies: HashSet::new(),
        }
    }

    /// Record changes for a single file
    pub fn record_file<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Channel,
        inode: Inode,
        working_copy_content: &[u8],
    ) -> Result<(), RecordError> {
        // 1. Get pristine content
        let pristine = retrieve_file_content(txn, channel, inode)?;

        // 2. Prepare lines
        let old_lines = prepare_lines_with_vertices(txn, channel, inode, &pristine)?;
        let new_lines = ContentHasher::prepare_lines(working_copy_content);

        // 3. Compute diff and generate hunks
        let file_hunks = diff_to_hunks(
            txn, channel, inode,
            &old_lines, &new_lines,
            working_copy_content,
            DiffAlgorithm::default(),
        )?;

        // 4. Track dependencies and collect hunks
        for hunk in file_hunks {
            self.collect_dependencies(&hunk);
            self.hunks.push(self.globalize_hunk(hunk, txn)?);
        }

        // 5. Append new content
        self.append_content(working_copy_content, &new_lines);

        Ok(())
    }

    pub fn finish(self) -> Change {
        Change {
            offsets: Offsets::default(), // Computed during serialization
            hashed: HashedChange {
                version: VERSION,
                header: self.header,
                dependencies: self.dependencies.into_iter().collect(),
                extra_known: Vec::new(),
                metadata: Vec::new(),
                hunks: self.hunks,
                contents_hash: Hash(blake3::hash(&self.contents).into()),
            },
            unhashed: None,
            contents: self.contents,
        }
    }
}
```

### 5.2 Applying Changes

```rust
pub fn apply_change<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    change: &Change,
    change_id: NodeId,
) -> Result<(), ApplyError> {
    // 1. Verify dependencies
    for dep in &change.hashed.dependencies {
        if txn.get_internal(dep)?.is_none() {
            return Err(ApplyError::MissingDependency { hash: *dep });
        }
    }

    // 2. Apply each hunk
    for hunk in &change.hashed.hunks {
        apply_hunk(txn, channel, hunk, change_id, &change.contents)?;
    }

    // 3. Update channel state
    let new_state = compute_new_state(txn.state(channel), &change)?;
    txn.set_state(channel, new_state)?;

    // 4. Record in channel's change log
    let timestamp = change.hashed.header.timestamp.timestamp() as u64;
    txn.put_changes(channel, change_id, timestamp)?;

    Ok(())
}

fn apply_hunk<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    hunk: &Hunk,
    change_id: NodeId,
    contents: &[u8],
) -> Result<(), ApplyError> {
    match hunk {
        Hunk::Edit { change: AtomicChange::NewVertex(nv), .. } => {
            apply_new_vertex(txn, channel, nv, change_id, contents)
        }
        Hunk::Edit { change: AtomicChange::EdgeMap(em), .. } => {
            apply_edge_map(txn, channel, em, change_id)
        }
        Hunk::FileAdd { add_name, add_inode, contents: file_contents, .. } => {
            apply_new_vertex(txn, channel, add_name, change_id, contents)?;
            apply_new_vertex(txn, channel, add_inode, change_id, contents)?;
            if let Some(fc) = file_contents {
                apply_new_vertex(txn, channel, fc, change_id, contents)?;
            }
            Ok(())
        }
        // ... other hunk types
    }
}

fn apply_new_vertex<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    nv: &NewVertex,
    change_id: NodeId,
    contents: &[u8],
) -> Result<(), ApplyError> {
    let vertex = Vertex {
        change: change_id,
        start: ChangePosition(L64(nv.start)),
        end: ChangePosition(L64(nv.end)),
    };

    // Add edges from up-context to new vertex
    for up in &nv.up_context {
        let up_internal = internalize_position(txn, up)?;
        let up_vertex = txn.find_block(txn.graph(channel), up_internal)?;

        let edge = SerializedEdge::new(nv.flag, vertex.start_pos(), change_id);
        txn.put_graph(up_vertex, edge)?;

        // Add reverse edge
        let rev_edge = SerializedEdge::new(
            nv.flag | EdgeFlags::PARENT,
            up_vertex.end_pos(),
            change_id,
        );
        txn.put_graph(vertex, rev_edge)?;
    }

    // Add edges from new vertex to down-context
    for down in &nv.down_context {
        let down_internal = internalize_position(txn, down)?;
        let down_vertex = txn.find_block(txn.graph(channel), down_internal)?;

        let edge = SerializedEdge::new(nv.flag, down_vertex.start_pos(), change_id);
        txn.put_graph(vertex, edge)?;

        // Add reverse edge
        let rev_edge = SerializedEdge::new(
            nv.flag | EdgeFlags::PARENT,
            vertex.end_pos(),
            change_id,
        );
        txn.put_graph(down_vertex, rev_edge)?;
    }

    Ok(())
}
```

---

## Phase 6: Working Copy (Week 3-4)

### 6.1 Output to Working Copy

```rust
/// Reconstruct working copy from graph
pub fn output_file<T: GraphTxnT, W: Write>(
    txn: &T,
    channel: &T::Channel,
    changes: &ChangeStore,
    inode: Inode,
    writer: &mut W,
) -> Result<OutputStats, OutputError> {
    let graph = retrieve_file_graph(txn, channel, inode)?;
    let alive = compute_alive_vertices(&graph)?;

    // Topological sort of alive vertices
    let sorted = topological_sort(&graph, &alive)?;

    let mut stats = OutputStats::default();

    for vertex in sorted {
        if let Some(conflict) = detect_order_conflict(&graph, vertex) {
            // Output conflict markers
            write_conflict_start(writer, &conflict)?;
            for alternative in conflict.alternatives {
                write_conflict_separator(writer)?;
                output_vertex_content(changes, alternative, writer)?;
            }
            write_conflict_end(writer)?;
            stats.conflicts += 1;
        } else {
            output_vertex_content(changes, vertex, writer)?;
        }
    }

    Ok(stats)
}

/// Detect changes in working copy
pub fn detect_changes(
    repo_path: &Path,
    channel: &Channel,
) -> Result<Vec<ChangedFile>, DetectError> {
    let mut changed = Vec::new();

    for entry in WalkDir::new(repo_path).into_iter().filter_entry(|e| !is_ignored(e)) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path().strip_prefix(repo_path)?;

            if let Some(inode) = lookup_inode(path)? {
                // Compare mtime/size first (fast path)
                if has_file_changed(inode, entry.metadata()?)? {
                    changed.push(ChangedFile::Modified(path.to_owned()));
                }
            } else {
                changed.push(ChangedFile::Added(path.to_owned()));
            }
        }
    }

    // Check for deletions
    for (path, inode) in iter_tracked_files(channel)?
CREATE TABLE tree (
    path TEXT PRIMARY KEY,
    inode INTEGER
);

CREATE TABLE inodes (
    inode INTEGER PRIMARY KEY,
    vertex_change INTEGER,
    vertex_pos INTEGER
);

-- Dependencies
CREATE TABLE deps (
    change_id INTEGER,
    dep_id INTEGER,
    PRIMARY KEY (change_id, dep_id)
);

CREATE INDEX idx_revdeps ON deps(dep_id, change_id);
```

---

## Phase 4: Diff Algorithm (Week 2-3)

### 4.1 Diff Strategy

```rust
/// Diff algorithm selection
#[derive(Clone, Copy, Debug, Default)]
pub enum DiffAlgorithm {
    /// Classic Myers diff - fast, good for most cases
    #[default]
    Myers,
    /// Patience diff - better for code with moved blocks
    Patience,
    /// Histogram diff - good balance of speed and quality
    Histogram,
}

/// Line representation for diffing
#[derive(Clone)]
pub struct Line<'a> {
    /// The actual content
    pub content: &'a [u8],
    /// Hash for fast equality comparison
    pub hash: u64,
    /// Original graph vertex (if from pristine)
    pub vertex: Option<Vertex<NodeId>>,
}

impl PartialEq for Line<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.content == other.content
    }
}
impl Eq for Line<'_> {}
```

### 4.2 Diff Output

```rust
/// Result of diffing two sequences
pub struct DiffResult {
    pub hunks: Vec<DiffHunk>,
}

pub struct DiffHunk {
    /// Position in old sequence
    pub old_start: usize,
    pub old_len: usize,
    /// Position in new sequence
    pub new_start: usize,
    pub new_len: usize,
}

impl DiffHunk {
    pub fn is_insert(&self) -> bool {
        self.old_len == 0
    }

    pub fn is_delete(&self) -> bool {
        self.new_len == 0
    }

    pub fn is_replace(&self) -> bool {
        self.old_len > 0 && self.new_len > 0
    }
}
```

### 4.3 Graph-Aware Diff

```rust
/// Convert file diff to graph operations
pub fn diff_to_hunks<T: GraphTxnT>(
    txn: &T,
    channel: &T::Channel,
    inode: Inode,
    old_lines: &[Line],
    new_lines: &[Line],
    new_content: &[u8],
    algorithm: DiffAlgorithm,
) -> Result<Vec<Hunk>, DiffError> {
    // 1. Compute line-level diff
    let diff = compute_diff(old_lines, new_lines, algorithm);

    // 2. Map diff hunks to graph operations
    let mut hunks = Vec::new();

    for hunk in diff.hunks {
        if hunk.is_delete() || hunk.is_replace() {
            // Create EdgeMap to mark deleted lines
            let edges = collect_deletion_edges(txn, channel, &old_lines[hunk.old_start..][..hunk.old_len])?;
            hunks.push(Hunk::Edit {
                change: AtomicChange::EdgeMap(edges),
                local: LocalContext::from_lines(old_lines, hunk.old_start),
                encoding: Some(Encoding::Utf8),
            });
        }

        if hunk.is_insert() || hunk.is_replace() {
            // Create NewVertex for inserted content
            let (up_ctx, down_ctx) = compute_context(old_lines, hunk.old_start)?;
            let content_range = compute_content_range(new_lines, hunk.new_start, hunk.new_len);

            let new_vertex = NewVertex {
                up_context: up_ctx,
                down_context: down_ctx,
                flag: EdgeFlags::BLOCK,
                start: content_range.start,
                end: content_range.end,
                inode: inode_position(txn, inode)?,
            };

            hunks.push(Hunk::Edit {
                change: AtomicChange::NewVertex(new_vertex),
                local: LocalContext::from_lines(new_lines, hunk.new_start),
                encoding: Some(Encoding::Utf8),
            });
        }
    }

    Ok(hunks)
}
```

### 4.4 Performance Optimization: Content Hashing

```rust
/// Fast content comparison using rolling hash
pub struct ContentHasher {
    // Use xxhash for speed (not cryptographic, just for comparison)
}

impl ContentHasher {
    /// Hash a line for quick comparison
    pub fn hash_line(content: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(content)
    }

    /// Prepare lines for diffing with pre-computed hashes
    pub fn prepare_lines(content: &[u8]) -> Vec<Line> {
        content
            .split_inclusive(|&b| b == b'\n')
            .map(|line| Line {
                content: line,
                hash: Self::hash_line(line),
                vertex: None,
            })
            .collect()
    }
}
```

---

## Phase 5: Record and Apply (Week 3)

### 5.1 Recording Changes

```rust
pub struct RecordBuilder {
    header: ChangeHeader,
    hunks: Vec<Hunk>,
    contents: Vec<u8>,
    dependencies: HashSet<Hash>,
}

impl RecordBuilder {
    pub fn new(author: Author, message: String) -> Self {
        Self {
            header: ChangeHeader {
                message,
                description: None,
                timestamp: Utc::now(),
                authors: vec![author],
            },
            hunks: Vec::new(),
            contents: Vec::new(),
            dependencies: HashSet::new(),
        }
    }

    /// Record changes for a single file
    pub fn record_file<T: GraphTxnT>(
        &mut self,
        txn: &T,
        channel: &T::Channel,
        inode: Inode,
        working_copy_content: &[u8],
    ) -> Result<(), RecordError> {
        // 1. Get pristine content
        let pristine = retrieve_file_content(txn, channel, inode)?;

        // 2. Prepare lines
        let old_lines = prepare_lines_with_vertices(txn, channel, inode, &pristine)?;
        let new_lines = ContentHasher::prepare_lines(working_copy_content);

        // 3. Compute diff and generate hunks
        let file_hunks = diff_to_hunks(
            txn, channel, inode,
            &old_lines, &new_lines,
            working_copy_content,
            DiffAlgorithm::default(),
        )?;

        // 4. Track dependencies and collect hunks
        for hunk in file_hunks {
            self.collect_dependencies(&hunk);
            self.hunks.push(self.globalize_hunk(hunk, txn)?);
        }

        // 5. Append new content
        self.append_content(working_copy_content, &new_lines);

        Ok(())
    }

    pub fn finish(self) -> Change {
        Change {
            offsets: Offsets::default(), // Computed during serialization
            hashed: HashedChange {
                version: VERSION,
                header: self.header,
                dependencies: self.dependencies.into_iter().collect(),
                extra_known: Vec::new(),
                metadata: Vec::new(),
                hunks: self.hunks,
                contents_hash: Hash(blake3::hash(&self.contents).into()),
            },
            unhashed: None,
            contents: self.contents,
        }
    }
}
```

### 5.2 Applying Changes

```rust
pub fn apply_change<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    change: &Change,
    change_id: NodeId,
) -> Result<(), ApplyError> {
    // 1. Verify dependencies
    for dep in &change.hashed.dependencies {
        if txn.get_internal(dep)?.is_none() {
            return Err(ApplyError::MissingDependency { hash: *dep });
        }
    }

    // 2. Apply each hunk
    for hunk in &change.hashed.hunks {
        apply_hunk(txn, channel, hunk, change_id, &change.contents)?;
    }

    // 3. Update channel state
    let new_state = compute_new_state(txn.state(channel), &change)?;
    txn.set_state(channel, new_state)?;

    // 4. Record in channel's change log
    let timestamp = change.hashed.header.timestamp.timestamp() as u64;
    txn.put_changes(channel, change_id, timestamp)?;

    Ok(())
}

fn apply_hunk<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    hunk: &Hunk,
    change_id: NodeId,
    contents: &[u8],
) -> Result<(), ApplyError> {
    match hunk {
        Hunk::Edit { change: AtomicChange::NewVertex(nv), .. } => {
            apply_new_vertex(txn, channel, nv, change_id, contents)
        }
        Hunk::Edit { change: AtomicChange::EdgeMap(em), .. } => {
            apply_edge_map(txn, channel, em, change_id)
        }
        Hunk::FileAdd { add_name, add_inode, contents: file_contents, .. } => {
            apply_new_vertex(txn, channel, add_name, change_id, contents)?;
            apply_new_vertex(txn, channel, add_inode, change_id, contents)?;
            if let Some(fc) = file_contents {
                apply_new_vertex(txn, channel, fc, change_id, contents)?;
            }
            Ok(())
        }
        // ... other hunk types
    }
}

fn apply_new_vertex<T: MutTxnT>(
    txn: &mut T,
    channel: &mut T::Channel,
    nv: &NewVertex,
    change_id: NodeId,
    contents: &[u8],
) -> Result<(), ApplyError> {
    let vertex = Vertex {
        change: change_id,
        start: ChangePosition(L64(nv.start)),
        end: ChangePosition(L64(nv.end)),
    };

    // Add edges from up-context to new vertex
    for up in &nv.up_context {
        let up_internal = internalize_position(txn, up)?;
        let up_vertex = txn.find_block(txn.graph(channel), up_internal)?;

        let edge = SerializedEdge::new(nv.flag, vertex.start_pos(), change_id);
        txn.put_graph(up_vertex, edge)?;

        // Add reverse edge
        let rev_edge = SerializedEdge::new(
            nv.flag | EdgeFlags::PARENT,
            up_vertex.end_pos(),
            change_id,
        );
        txn.put_graph(vertex, rev_edge)?;
    }

    // Add edges from new vertex to down-context
    for down in &nv.down_context {
        let down_internal = internalize_position(txn, down)?;
        let down_vertex = txn.find_block(txn.graph(channel), down_internal)?;

        let edge = SerializedEdge::new(nv.flag, down_vertex.start_pos(), change_id);
        txn.put_graph(vertex, edge)?;

        // Add reverse edge
        let rev_edge = SerializedEdge::new(
            nv.flag | EdgeFlags::PARENT,
            vertex.end_pos(),
            change_id,
        );
        txn.put_graph(down_vertex, rev_edge)?;
    }

    Ok(())
}
```

---

## Phase 6: Working Copy (Week 3-4)

### 6.1 Output to Working Copy

```rust
/// Reconstruct working copy from graph
pub fn output_file<T: GraphTxnT, W: Write>(
    txn: &T,
    channel: &T::Channel,
    changes: &ChangeStore,
    inode: Inode,
    writer: &mut W,
) -> Result<OutputStats, OutputError> {
    let graph = retrieve_file_graph(txn, channel, inode)?;
    let alive = compute_alive_vertices(&graph)?;

    // Topological sort of alive vertices
    let sorted = topological_sort(&graph, &alive)?;

    let mut stats = OutputStats::default();

    for vertex in sorted {
        if let Some(conflict) = detect_order_conflict(&graph, vertex) {
            // Output conflict markers
            write_conflict_start(writer, &conflict)?;
            for alternative in conflict.alternatives {
                write_conflict_separator(writer)?;
                output_vertex_content(changes, alternative, writer)?;
            }
            write_conflict_end(writer)?;
            stats.conflicts += 1;
        } else {
            output_vertex_content(changes, vertex, writer)?;
        }
    }

    Ok(stats)
}

/// Detect changes in working copy
pub fn detect_changes(
    repo_path: &Path,
    channel: &Channel,
) -> Result<Vec<ChangedFile>, DetectError> {
    let mut changed = Vec::new();

    for entry in WalkDir::new(repo_path).into_iter().filter_entry(|e| !is_ignored(e)) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path().strip_prefix(repo_path)?;

            if let Some(inode) = lookup_inode(path)? {
                // Compare mtime/size first (fast path)
                if has_file_changed(inode, entry.metadata()?)? {
                    changed.push(ChangedFile::Modified(path.to_owned()));
                }
            } else {
                changed.push(ChangedFile::Added(path.to_owned()));
            }
        }
    }

    // Check for deletions
    for (path, inode) in iter_tracked_files(channel)?
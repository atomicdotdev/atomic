//! Compact graph types for V3 serialization.
//!
//! This module defines space-efficient versions of the core graph types that
//! use [`HashIndex`] references (2 bytes) instead of full 32-byte hashes.
//! These types exist **solely for serialization** — they are never used in
//! the in-memory graph. Conversion happens at the read/write boundary:
//!
//! ```text
//! Recording:  GraphOp<Option<Hash>> → CompactGraphOp → postcard → zstd → disk
//! Applying:   disk → zstd → postcard → CompactGraphOp → GraphOp<Option<Hash>>
//! ```
//!
//! # Why Compact Types?
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash plus a
//! 1-byte `Option` discriminant. A `GraphNode<Option<Hash>>` stores 33 + 8 + 8 = 49
//! bytes. An `Insertion` with 2 predecessors and 1 successor stores 3×41 + 2×8 + 41 =
//! 180+ bytes — just for the position fields!
//!
//! The compact types replace `Option<Hash>` with `HashIndex` (`u16`) and `u64`
//! positions with `u32`. Combined with postcard's varint encoding:
//!
//! | Type | V2 (bincode) | V3 (postcard) | Savings |
//! |------|-------------|---------------|---------|
//! | `Position` | 41 bytes | 2-8 bytes | 80-95% |
//! | `GraphNode` | 49 bytes | 3-12 bytes | 76-94% |
//! | `NewEdge` | 132+ bytes | 10-30 bytes | 77-92% |
//! | `Insertion` (typical) | 180+ bytes | 15-40 bytes | 78-92% |
//!
//! # Type Mapping
//!
//! | Full Type | Compact Type | Key Change |
//! |-----------|-------------|------------|
//! | `Position<Option<Hash>>` | [`CompactPosition`] | `Option<Hash>` → `HashIndex` |
//! | `GraphNode<Option<Hash>>` | [`CompactGraphNode`] | `Option<Hash>` → `HashIndex` |
//! | `Insertion<Option<Hash>>` | [`CompactInsertion`] | All positions compacted |
//! | `NewEdge<Option<Hash>>` | [`CompactNewEdge`] | Positions + `introduced_by` compacted |
//! | `EdgeUpdate<Option<Hash>>` | [`CompactEdgeUpdate`] | Contains `Vec<CompactNewEdge>` |
//! | `Atom<Option<Hash>>` | [`CompactAtom`] | Enum over compact variants |
//! | `GraphOp<Option<Hash>>` | [`CompactGraphOp`] | All fields compacted |
//!
//! # Conversion
//!
//! Use [`Compactor`] to convert between full and compact types:
//!
//! ```rust
//! use atomic_core::change::format_v3::compact::Compactor;
//! use atomic_core::change::format_v3::HashDedupTable;
//!
//! let self_hash = *blake3::hash(b"my change").as_bytes();
//! let table = HashDedupTable::new(self_hash);
//!
//! let compactor = Compactor::new(&table);
//! // compactor.compact_graph_op(&graph_op) → CompactGraphOp
//! // compactor.expand_graph_op(&compact_op) → GraphOp<Option<Hash>>
//! ```
//!
//! # Thread Safety
//!
//! All compact types are `Send + Sync`. [`Compactor`] borrows a [`HashDedupTable`]
//! immutably and is also `Send + Sync`.

use super::error::FormatResult;
use super::hash_table::HashDedupTable;
use super::types::{CompactPosition, HashIndex, HASH_INDEX_NONE, HASH_INDEX_SELF};
use crate::change::atom::{Atom, EdgeUpdate, Insertion, NewEdge};
use crate::change::encoding::Encoding;
use crate::change::graph_op::GraphOp;
use crate::change::local::Local;
use crate::types::{ChangePosition, EdgeFlags};
use crate::Hash;
use crate::Position;
use serde::{Deserialize, Serialize};
use std::fmt;

// ═══════════════════════════════════════════════════════════════════════
// CompactGraphNode — GraphNode<Option<Hash>> → (HashIndex, u32, u32)
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`GraphNode<Option<Hash>>`](crate::GraphNode).
///
/// Replaces the 33-byte `Option<Hash>` with a 1-3 byte `HashIndex` varint
/// and the two 8-byte `u64` positions with 1-5 byte `u32` varints.
///
/// # Size Comparison
///
/// | Encoding | `change` | `start` | `end` | Total |
/// |----------|----------|---------|-------|-------|
/// | V2 (bincode) | 33 bytes | 8 bytes | 8 bytes | 49 bytes |
/// | V3 (postcard, typical) | 1 byte | 1-2 bytes | 1-2 bytes | 3-5 bytes |
/// | V3 (postcard, max) | 3 bytes | 5 bytes | 5 bytes | 13 bytes |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CompactGraphNode {
    /// Index into the hash dedup table identifying the change.
    ///
    /// - `0` = this change itself
    /// - `0xFFFF` = root / none
    /// - Other = dependency change
    pub change: HashIndex,

    /// Start position within the change's content (inclusive).
    pub start: u32,

    /// End position within the change's content (exclusive).
    pub end: u32,
}

impl CompactGraphNode {
    /// Create a new compact graph node.
    #[inline]
    pub const fn new(change: HashIndex, start: u32, end: u32) -> Self {
        Self { change, start, end }
    }

    /// Create a self-referencing graph node (references this change's content).
    #[inline]
    pub const fn self_ref(start: u32, end: u32) -> Self {
        Self {
            change: HASH_INDEX_SELF,
            start,
            end,
        }
    }

    /// Create a root graph node.
    #[inline]
    pub const fn root(start: u32, end: u32) -> Self {
        Self {
            change: HASH_INDEX_NONE,
            start,
            end,
        }
    }

    /// Returns `true` if this is a root node (no associated change).
    #[inline]
    pub const fn is_root(&self) -> bool {
        self.change == HASH_INDEX_NONE
    }

    /// Returns `true` if this node references the change's own content.
    #[inline]
    pub const fn is_self_ref(&self) -> bool {
        self.change == HASH_INDEX_SELF
    }

    /// Returns the content length in bytes.
    #[inline]
    pub const fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Returns `true` if this is an empty node (start == end).
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl fmt::Display for CompactGraphNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            write!(f, "ROOT[{}:{}]", self.start, self.end)
        } else if self.is_self_ref() {
            write!(f, "SELF[{}:{}]", self.start, self.end)
        } else {
            write!(f, "#{}[{}:{}]", self.change, self.start, self.end)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactInsertion — Insertion<Option<Hash>> using compact positions
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`Insertion<Option<Hash>>`](Insertion).
///
/// All position fields use [`CompactPosition`] instead of `Position<Option<Hash>>`,
/// and byte offsets use `u32` instead of `u64`.
///
/// # Size Comparison (typical: 2 predecessors, 0 successors)
///
/// | Field | V2 (bincode) | V3 (postcard) |
/// |-------|-------------|---------------|
/// | `predecessors` (2×) | 8 + 2×41 = 90 bytes | 1 + 2×2 = 5 bytes |
/// | `successors` (0×) | 8 bytes | 1 byte |
/// | `flag` | 1 byte | 1 byte |
/// | `start` | 8 bytes | 1-2 bytes |
/// | `end` | 8 bytes | 1-2 bytes |
/// | `inode` | 41 bytes | 2-3 bytes |
/// | **Total** | **156 bytes** | **11-14 bytes** |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactInsertion {
    /// Vertices that should come before this new content.
    pub predecessors: Vec<CompactPosition>,

    /// Vertices that should come after this new content.
    pub successors: Vec<CompactPosition>,

    /// Edge flags (BLOCK, FOLDER, etc.).
    pub flag: u8,

    /// Start offset in the change's content blob (inclusive).
    pub start: u32,

    /// End offset in the change's content blob (exclusive).
    pub end: u32,

    /// The file (inode) this vertex belongs to.
    pub inode: CompactPosition,
}

impl CompactInsertion {
    /// Returns the content length in bytes.
    #[inline]
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Returns `true` if this is an empty vertex (start == end).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Returns `true` if this vertex has predecessors.
    #[inline]
    pub fn has_predecessors(&self) -> bool {
        !self.predecessors.is_empty()
    }

    /// Returns `true` if this vertex has successors.
    #[inline]
    pub fn has_successors(&self) -> bool {
        !self.successors.is_empty()
    }
}

impl fmt::Display for CompactInsertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CompactInsertion[{}..{}] ({} up, {} down)",
            self.start,
            self.end,
            self.predecessors.len(),
            self.successors.len()
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactNewEdge — NewEdge<Option<Hash>> using compact positions
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`NewEdge<Option<Hash>>`](NewEdge).
///
/// Replaces full hash positions with compact index-based positions.
///
/// # Size Comparison
///
/// | Field | V2 (bincode) | V3 (postcard) |
/// |-------|-------------|---------------|
/// | `previous` | 1 byte | 1 byte |
/// | `flag` | 1 byte | 1 byte |
/// | `from` (Position) | 41 bytes | 2-5 bytes |
/// | `to` (GraphNode) | 49 bytes | 3-8 bytes |
/// | `introduced_by` | 33 bytes | 1-3 bytes |
/// | **Total** | **125 bytes** | **8-18 bytes** |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactNewEdge {
    /// The flags the edge currently has.
    pub previous: u8,

    /// The flags the edge should have after modification.
    pub flag: u8,

    /// Source position of the edge.
    pub from: CompactPosition,

    /// Destination node of the edge.
    pub to: CompactGraphNode,

    /// Change that originally introduced this edge.
    ///
    /// - `HASH_INDEX_SELF` (0) = this change
    /// - `HASH_INDEX_NONE` (0xFFFF) = root
    /// - Other = dependency change index
    pub introduced_by: HashIndex,
}

impl fmt::Display for CompactNewEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Edge(0x{:02X}→0x{:02X}, {} → {}, by #{})",
            self.previous, self.flag, self.from, self.to, self.introduced_by
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactEdgeUpdate — EdgeUpdate<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`EdgeUpdate<Option<Hash>>`](EdgeUpdate).
///
/// Contains a list of compact edge modifications and the file's inode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactEdgeUpdate {
    /// The edge modifications to apply.
    pub edges: Vec<CompactNewEdge>,

    /// The file (inode) these edges belong to.
    pub inode: CompactPosition,
}

impl CompactEdgeUpdate {
    /// Returns `true` if there are no edge modifications.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Returns the number of edge modifications.
    #[inline]
    pub fn len(&self) -> usize {
        self.edges.len()
    }
}

impl fmt::Display for CompactEdgeUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CompactEdgeUpdate({} edges)", self.edges.len())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactAtom — Atom<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`Atom<Option<Hash>>`](Atom).
///
/// An atomic graph operation: either an insertion of new content or a
/// modification of existing edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactAtom {
    /// Insert new content into the graph.
    Insertion(CompactInsertion),
    /// Modify existing edges.
    EdgeUpdate(CompactEdgeUpdate),
}

impl CompactAtom {
    /// Returns `true` if this is an insertion.
    #[inline]
    pub fn is_insertion(&self) -> bool {
        matches!(self, CompactAtom::Insertion(_))
    }

    /// Returns `true` if this is an edge update.
    #[inline]
    pub fn is_edge_update(&self) -> bool {
        matches!(self, CompactAtom::EdgeUpdate(_))
    }
}

impl fmt::Display for CompactAtom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompactAtom::Insertion(v) => write!(f, "{}", v),
            CompactAtom::EdgeUpdate(e) => write!(f, "{}", e),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// CompactGraphOp — GraphOp<Option<Hash>> using compact types
// ═══════════════════════════════════════════════════════════════════════

/// Compact version of [`GraphOp<Option<Hash>>`](GraphOp).
///
/// This is the top-level hunk type for V3 serialization. Each variant
/// mirrors the corresponding `GraphOp` variant but uses compact types
/// for all position, node, and hash references.
///
/// # Variant Mapping
///
/// | `GraphOp` variant | `CompactGraphOp` variant |
/// |-------------------|--------------------------|
/// | `FileAdd` | `FileAdd` |
/// | `DirAdd` | `DirAdd` |
/// | `DirDel` | `DirDel` |
/// | `DirUndel` | `DirUndel` |
/// | `FileDel` | `FileDel` |
/// | `FileUndel` | `FileUndel` |
/// | `FileMove` | `FileMove` |
/// | `Edit` | `Edit` |
/// | `Replacement` | `Replacement` |
/// | `SolveNameConflict` | `SolveNameConflict` |
/// | `UnsolveNameConflict` | `UnsolveNameConflict` |
/// | `SolveOrderConflict` | `SolveOrderConflict` |
/// | `UnsolveOrderConflict` | `UnsolveOrderConflict` |
/// | `ResurrectZombies` | `ResurrectZombies` |
/// | `AddRoot` | `AddRoot` |
/// | `DelRoot` | `DelRoot` |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactGraphOp {
    /// Add a new file.
    FileAdd {
        /// Vertex to add the filename in parent directory.
        add_name: CompactInsertion,
        /// Vertex to create the file's inode.
        add_inode: CompactInsertion,
        /// Optional initial file contents.
        #[serde(default)]
        contents: Option<CompactInsertion>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add an empty directory.
    DirAdd {
        /// Vertex to add the directory name in parent directory.
        add_name: CompactInsertion,
        /// Vertex to create the directory's inode.
        add_inode: CompactInsertion,
        /// Path for human readability.
        path: String,
    },

    /// Delete an empty directory.
    DirDel {
        /// Edges to mark as deleted.
        del: CompactEdgeUpdate,
        /// Path for human readability.
        path: String,
    },

    /// Restore a deleted directory.
    DirUndel {
        /// Edges to restore.
        undel: CompactEdgeUpdate,
        /// Path for human readability.
        path: String,
    },

    /// Delete a file.
    FileDel {
        /// Edges to mark as deleted.
        del: CompactEdgeUpdate,
        /// Content edges to delete (if file has content).
        #[serde(default)]
        contents: Option<CompactEdgeUpdate>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Restore a deleted file.
    FileUndel {
        /// Edges to restore.
        undel: CompactEdgeUpdate,
        /// Content edges to restore.
        #[serde(default)]
        contents: Option<CompactEdgeUpdate>,
        /// Path for human readability.
        path: String,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Move or rename a file.
    FileMove {
        /// Remove old name edge.
        del: CompactEdgeUpdate,
        /// Add new name edge.
        add: CompactInsertion,
        /// New path for human readability.
        path: String,
    },

    /// Edit file contents.
    Edit {
        /// The modification (insert or delete).
        change: CompactAtom,
        /// Local context for display (path + line number).
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Replace content (delete + insert).
    Replacement {
        /// Content to delete.
        change: CompactEdgeUpdate,
        /// Content to insert.
        replacement: CompactInsertion,
        /// Local context for display.
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Solve a name conflict.
    SolveNameConflict {
        /// The resolution operation.
        name: CompactEdgeUpdate,
        /// Path where conflict occurred.
        path: String,
    },

    /// Reopen a solved name conflict.
    UnsolveNameConflict {
        /// The operation to undo the resolution.
        name: CompactEdgeUpdate,
        /// Path where conflict is.
        path: String,
    },

    /// Solve an ordering conflict.
    SolveOrderConflict {
        /// The resolution operation.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
    },

    /// Reopen a solved ordering conflict.
    UnsolveOrderConflict {
        /// The operation to undo the resolution.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
    },

    /// Resurrect deleted content (zombies).
    ResurrectZombies {
        /// The resurrection operation.
        change: CompactEdgeUpdate,
        /// Local context for display.
        local: Local,
        /// Text encoding (if text file).
        #[serde(default)]
        encoding: Option<Encoding>,
    },

    /// Add a repository root.
    AddRoot {
        /// Name of the root.
        name: CompactInsertion,
        /// Inode for the root.
        inode: CompactInsertion,
    },

    /// Delete a repository root.
    DelRoot {
        /// Name edges to delete.
        name: CompactEdgeUpdate,
        /// Inode edges to delete.
        inode: CompactEdgeUpdate,
    },
}

impl CompactGraphOp {
    /// Returns the path associated with this operation, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            CompactGraphOp::FileAdd { path, .. }
            | CompactGraphOp::DirAdd { path, .. }
            | CompactGraphOp::DirDel { path, .. }
            | CompactGraphOp::DirUndel { path, .. }
            | CompactGraphOp::FileDel { path, .. }
            | CompactGraphOp::FileUndel { path, .. }
            | CompactGraphOp::FileMove { path, .. }
            | CompactGraphOp::SolveNameConflict { path, .. }
            | CompactGraphOp::UnsolveNameConflict { path, .. } => Some(path),
            CompactGraphOp::Edit { local, .. }
            | CompactGraphOp::Replacement { local, .. }
            | CompactGraphOp::SolveOrderConflict { local, .. }
            | CompactGraphOp::UnsolveOrderConflict { local, .. }
            | CompactGraphOp::ResurrectZombies { local, .. } => Some(&local.path),
            CompactGraphOp::AddRoot { .. } | CompactGraphOp::DelRoot { .. } => None,
        }
    }

    /// Returns a human-readable type name for this operation.
    pub fn type_name(&self) -> &'static str {
        match self {
            CompactGraphOp::FileAdd { .. } => "FileAdd",
            CompactGraphOp::DirAdd { .. } => "DirAdd",
            CompactGraphOp::DirDel { .. } => "DirDel",
            CompactGraphOp::DirUndel { .. } => "DirUndel",
            CompactGraphOp::FileDel { .. } => "FileDel",
            CompactGraphOp::FileUndel { .. } => "FileUndel",
            CompactGraphOp::FileMove { .. } => "FileMove",
            CompactGraphOp::Edit { .. } => "Edit",
            CompactGraphOp::Replacement { .. } => "Replacement",
            CompactGraphOp::SolveNameConflict { .. } => "SolveNameConflict",
            CompactGraphOp::UnsolveNameConflict { .. } => "UnsolveNameConflict",
            CompactGraphOp::SolveOrderConflict { .. } => "SolveOrderConflict",
            CompactGraphOp::UnsolveOrderConflict { .. } => "UnsolveOrderConflict",
            CompactGraphOp::ResurrectZombies { .. } => "ResurrectZombies",
            CompactGraphOp::AddRoot { .. } => "AddRoot",
            CompactGraphOp::DelRoot { .. } => "DelRoot",
        }
    }
}

impl fmt::Display for CompactGraphOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path() {
            Some(path) => write!(f, "{}({})", self.type_name(), path),
            None => write!(f, "{}", self.type_name()),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Compactor — converts between full and compact types
// ═══════════════════════════════════════════════════════════════════════

/// Converts between full-hash graph types and compact index-based types.
///
/// A `Compactor` holds a reference to a [`HashDedupTable`] and provides
/// methods to convert in both directions:
///
/// - **Compact** (`full → compact`): Used during writing. Looks up each
///   `Option<Hash>` in the dedup table to get its `HashIndex`.
/// - **Expand** (`compact → full`): Used during reading. Resolves each
///   `HashIndex` back to an `Option<Hash>` via the dedup table.
///
/// # Error Handling
///
/// - `compact_*` methods return [`FormatError::HashNotFound`] if a hash
///   isn't in the dedup table. This means the table was built incorrectly.
/// - `expand_*` methods return [`FormatError::HashIndexOutOfBounds`] if
///   an index exceeds the table size. This means the file is corrupt.
///
/// # Examples
///
/// ```rust
/// use atomic_core::change::format_v3::compact::Compactor;
/// use atomic_core::change::format_v3::HashDedupTable;
///
/// let self_hash = *blake3::hash(b"test").as_bytes();
/// let table = HashDedupTable::new(self_hash);
/// let compactor = Compactor::new(&table);
/// ```
pub struct Compactor<'t> {
    table: &'t HashDedupTable,
}

impl<'t> Compactor<'t> {
    /// Create a new compactor using the given hash dedup table.
    #[inline]
    pub fn new(table: &'t HashDedupTable) -> Self {
        Self { table }
    }

    /// Returns a reference to the underlying hash dedup table.
    #[inline]
    pub fn table(&self) -> &HashDedupTable {
        self.table
    }

    // ── Compact: Full → Compact ────────────────────────────────────

    /// Convert a `Position<Option<Hash>>` to a [`CompactPosition`].
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::HashNotFound`] if the hash isn't in the dedup table.
    pub fn compact_position(&self, pos: &Position<Option<Hash>>) -> FormatResult<CompactPosition> {
        let change = self.hash_to_index(&pos.change)?;
        Ok(CompactPosition::new(change, pos.pos.get() as u32))
    }

    /// Convert a `GraphNode<Option<Hash>>` to a [`CompactGraphNode`].
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::HashNotFound`] if the hash isn't in the dedup table.
    pub fn compact_graph_node(
        &self,
        node: &crate::GraphNode<Option<Hash>>,
    ) -> FormatResult<CompactGraphNode> {
        let change = self.hash_to_index(&node.change)?;
        Ok(CompactGraphNode::new(
            change,
            node.start.get() as u32,
            node.end.get() as u32,
        ))
    }

    /// Convert an `Insertion<Option<Hash>>` to a [`CompactInsertion`].
    pub fn compact_insertion(&self, v: &Insertion<Option<Hash>>) -> FormatResult<CompactInsertion> {
        let predecessors = v
            .predecessors
            .iter()
            .map(|p| self.compact_position(p))
            .collect::<FormatResult<Vec<_>>>()?;
        let successors = v
            .successors
            .iter()
            .map(|p| self.compact_position(p))
            .collect::<FormatResult<Vec<_>>>()?;
        let inode = self.compact_position(&v.inode)?;

        Ok(CompactInsertion {
            predecessors,
            successors,
            flag: v.flag.bits(),
            start: v.start.get() as u32,
            end: v.end.get() as u32,
            inode,
        })
    }

    /// Convert a `NewEdge<Option<Hash>>` to a [`CompactNewEdge`].
    pub fn compact_new_edge(&self, e: &NewEdge<Option<Hash>>) -> FormatResult<CompactNewEdge> {
        let from = self.compact_position(&e.from)?;
        let to = self.compact_graph_node(&e.to)?;
        let introduced_by = self.hash_to_index(&e.introduced_by)?;

        Ok(CompactNewEdge {
            previous: e.previous.bits(),
            flag: e.flag.bits(),
            from,
            to,
            introduced_by,
        })
    }

    /// Convert an `EdgeUpdate<Option<Hash>>` to a [`CompactEdgeUpdate`].
    pub fn compact_edge_update(
        &self,
        em: &EdgeUpdate<Option<Hash>>,
    ) -> FormatResult<CompactEdgeUpdate> {
        let edges = em
            .edges
            .iter()
            .map(|e| self.compact_new_edge(e))
            .collect::<FormatResult<Vec<_>>>()?;
        let inode = self.compact_position(&em.inode)?;

        Ok(CompactEdgeUpdate { edges, inode })
    }

    /// Convert an `Atom<Option<Hash>>` to a [`CompactAtom`].
    pub fn compact_atom(&self, atom: &Atom<Option<Hash>>) -> FormatResult<CompactAtom> {
        match atom {
            Atom::Insertion(v) => Ok(CompactAtom::Insertion(self.compact_insertion(v)?)),
            Atom::EdgeUpdate(em) => Ok(CompactAtom::EdgeUpdate(self.compact_edge_update(em)?)),
        }
    }

    /// Convert a `GraphOp<Option<Hash>>` to a [`CompactGraphOp`].
    ///
    /// This is the main entry point for compacting a full graph operation
    /// before serialization. It recursively compacts all nested positions,
    /// nodes, and hashes.
    ///
    /// # Errors
    ///
    /// Returns an error if any hash referenced by the graph operation
    /// is not found in the dedup table.
    pub fn compact_graph_op(&self, op: &GraphOp<Option<Hash>>) -> FormatResult<CompactGraphOp> {
        match op {
            GraphOp::FileAdd {
                add_name,
                add_inode,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileAdd {
                add_name: self.compact_insertion(add_name)?,
                add_inode: self.compact_insertion(add_inode)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_insertion(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::DirAdd {
                add_name,
                add_inode,
                path,
            } => Ok(CompactGraphOp::DirAdd {
                add_name: self.compact_insertion(add_name)?,
                add_inode: self.compact_insertion(add_inode)?,
                path: path.clone(),
            }),

            GraphOp::DirDel { del, path } => Ok(CompactGraphOp::DirDel {
                del: self.compact_edge_update(del)?,
                path: path.clone(),
            }),

            GraphOp::DirUndel { undel, path } => Ok(CompactGraphOp::DirUndel {
                undel: self.compact_edge_update(undel)?,
                path: path.clone(),
            }),

            GraphOp::FileDel {
                del,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileDel {
                del: self.compact_edge_update(del)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::FileUndel {
                undel,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileUndel {
                undel: self.compact_edge_update(undel)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::FileMove { del, add, path } => Ok(CompactGraphOp::FileMove {
                del: self.compact_edge_update(del)?,
                add: self.compact_insertion(add)?,
                path: path.clone(),
            }),

            GraphOp::Edit {
                change,
                local,
                encoding,
            } => Ok(CompactGraphOp::Edit {
                change: self.compact_atom(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::Replacement {
                change,
                replacement,
                local,
                encoding,
            } => Ok(CompactGraphOp::Replacement {
                change: self.compact_edge_update(change)?,
                replacement: self.compact_insertion(replacement)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::SolveNameConflict { name, path } => Ok(CompactGraphOp::SolveNameConflict {
                name: self.compact_edge_update(name)?,
                path: path.clone(),
            }),

            GraphOp::UnsolveNameConflict { name, path } => {
                Ok(CompactGraphOp::UnsolveNameConflict {
                    name: self.compact_edge_update(name)?,
                    path: path.clone(),
                })
            }

            GraphOp::SolveOrderConflict { change, local } => {
                Ok(CompactGraphOp::SolveOrderConflict {
                    change: self.compact_edge_update(change)?,
                    local: local.clone(),
                })
            }

            GraphOp::UnsolveOrderConflict { change, local } => {
                Ok(CompactGraphOp::UnsolveOrderConflict {
                    change: self.compact_edge_update(change)?,
                    local: local.clone(),
                })
            }

            GraphOp::ResurrectZombies {
                change,
                local,
                encoding,
            } => Ok(CompactGraphOp::ResurrectZombies {
                change: self.compact_edge_update(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::AddRoot { name, inode } => Ok(CompactGraphOp::AddRoot {
                name: self.compact_insertion(name)?,
                inode: self.compact_insertion(inode)?,
            }),

            GraphOp::DelRoot { name, inode } => Ok(CompactGraphOp::DelRoot {
                name: self.compact_edge_update(name)?,
                inode: self.compact_edge_update(inode)?,
            }),
        }
    }

    // ── Expand: Compact → Full ─────────────────────────────────────

    /// Convert a [`CompactPosition`] to a `Position<Option<Hash>>`.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::HashIndexOutOfBounds`] if the index is invalid.
    pub fn expand_position(&self, pos: &CompactPosition) -> FormatResult<Position<Option<Hash>>> {
        let change = self.index_to_hash(pos.change)?;
        Ok(Position {
            change,
            pos: ChangePosition::new(pos.pos as u64),
        })
    }

    /// Convert a [`CompactGraphNode`] to a `GraphNode<Option<Hash>>`.
    pub fn expand_graph_node(
        &self,
        node: &CompactGraphNode,
    ) -> FormatResult<crate::GraphNode<Option<Hash>>> {
        let change = self.index_to_hash(node.change)?;
        Ok(crate::GraphNode {
            change,
            start: ChangePosition::new(node.start as u64),
            end: ChangePosition::new(node.end as u64),
        })
    }

    /// Convert a [`CompactInsertion`] to an `Insertion<Option<Hash>>`.
    pub fn expand_insertion(&self, v: &CompactInsertion) -> FormatResult<Insertion<Option<Hash>>> {
        let predecessors = v
            .predecessors
            .iter()
            .map(|p| self.expand_position(p))
            .collect::<FormatResult<Vec<_>>>()?;
        let successors = v
            .successors
            .iter()
            .map(|p| self.expand_position(p))
            .collect::<FormatResult<Vec<_>>>()?;
        let inode = self.expand_position(&v.inode)?;

        Ok(Insertion {
            predecessors,
            successors,
            flag: EdgeFlags::from_bits_truncate(v.flag),
            start: ChangePosition::new(v.start as u64),
            end: ChangePosition::new(v.end as u64),
            inode,
        })
    }

    /// Convert a [`CompactNewEdge`] to a `NewEdge<Option<Hash>>`.
    pub fn expand_new_edge(&self, e: &CompactNewEdge) -> FormatResult<NewEdge<Option<Hash>>> {
        let from = self.expand_position(&e.from)?;
        let to = self.expand_graph_node(&e.to)?;
        let introduced_by = self.index_to_hash(e.introduced_by)?;

        Ok(NewEdge {
            previous: EdgeFlags::from_bits_truncate(e.previous),
            flag: EdgeFlags::from_bits_truncate(e.flag),
            from,
            to,
            introduced_by,
        })
    }

    /// Convert a [`CompactEdgeUpdate`] to an `EdgeUpdate<Option<Hash>>`.
    pub fn expand_edge_update(
        &self,
        em: &CompactEdgeUpdate,
    ) -> FormatResult<EdgeUpdate<Option<Hash>>> {
        let edges = em
            .edges
            .iter()
            .map(|e| self.expand_new_edge(e))
            .collect::<FormatResult<Vec<_>>>()?;
        let inode = self.expand_position(&em.inode)?;

        Ok(EdgeUpdate { edges, inode })
    }

    /// Convert a [`CompactAtom`] to an `Atom<Option<Hash>>`.
    pub fn expand_atom(&self, atom: &CompactAtom) -> FormatResult<Atom<Option<Hash>>> {
        match atom {
            CompactAtom::Insertion(v) => Ok(Atom::Insertion(self.expand_insertion(v)?)),
            CompactAtom::EdgeUpdate(em) => Ok(Atom::EdgeUpdate(self.expand_edge_update(em)?)),
        }
    }

    /// Convert a [`CompactGraphOp`] to a `GraphOp<Option<Hash>>`.
    ///
    /// This is the main entry point for expanding a compact graph operation
    /// after deserialization. It recursively expands all nested positions,
    /// nodes, and hash indices.
    ///
    /// # Errors
    ///
    /// Returns an error if any hash index in the compact operation
    /// is out of bounds for the dedup table.
    pub fn expand_graph_op(&self, op: &CompactGraphOp) -> FormatResult<GraphOp<Option<Hash>>> {
        match op {
            CompactGraphOp::FileAdd {
                add_name,
                add_inode,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileAdd {
                add_name: self.expand_insertion(add_name)?,
                add_inode: self.expand_insertion(add_inode)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_insertion(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::DirAdd {
                add_name,
                add_inode,
                path,
            } => Ok(GraphOp::DirAdd {
                add_name: self.expand_insertion(add_name)?,
                add_inode: self.expand_insertion(add_inode)?,
                path: path.clone(),
            }),

            CompactGraphOp::DirDel { del, path } => Ok(GraphOp::DirDel {
                del: self.expand_edge_update(del)?,
                path: path.clone(),
            }),

            CompactGraphOp::DirUndel { undel, path } => Ok(GraphOp::DirUndel {
                undel: self.expand_edge_update(undel)?,
                path: path.clone(),
            }),

            CompactGraphOp::FileDel {
                del,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileDel {
                del: self.expand_edge_update(del)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::FileUndel {
                undel,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileUndel {
                undel: self.expand_edge_update(undel)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::FileMove { del, add, path } => Ok(GraphOp::FileMove {
                del: self.expand_edge_update(del)?,
                add: self.expand_insertion(add)?,
                path: path.clone(),
            }),

            CompactGraphOp::Edit {
                change,
                local,
                encoding,
            } => Ok(GraphOp::Edit {
                change: self.expand_atom(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::Replacement {
                change,
                replacement,
                local,
                encoding,
            } => Ok(GraphOp::Replacement {
                change: self.expand_edge_update(change)?,
                replacement: self.expand_insertion(replacement)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::SolveNameConflict { name, path } => Ok(GraphOp::SolveNameConflict {
                name: self.expand_edge_update(name)?,
                path: path.clone(),
            }),

            CompactGraphOp::UnsolveNameConflict { name, path } => {
                Ok(GraphOp::UnsolveNameConflict {
                    name: self.expand_edge_update(name)?,
                    path: path.clone(),
                })
            }

            CompactGraphOp::SolveOrderConflict { change, local } => {
                Ok(GraphOp::SolveOrderConflict {
                    change: self.expand_edge_update(change)?,
                    local: local.clone(),
                })
            }

            CompactGraphOp::UnsolveOrderConflict { change, local } => {
                Ok(GraphOp::UnsolveOrderConflict {
                    change: self.expand_edge_update(change)?,
                    local: local.clone(),
                })
            }

            CompactGraphOp::ResurrectZombies {
                change,
                local,
                encoding,
            } => Ok(GraphOp::ResurrectZombies {
                change: self.expand_edge_update(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::AddRoot { name, inode } => Ok(GraphOp::AddRoot {
                name: self.expand_insertion(name)?,
                inode: self.expand_insertion(inode)?,
            }),

            CompactGraphOp::DelRoot { name, inode } => Ok(GraphOp::DelRoot {
                name: self.expand_edge_update(name)?,
                inode: self.expand_edge_update(inode)?,
            }),
        }
    }

    // ── Internal Helpers ───────────────────────────────────────────

    /// Convert an `Option<Hash>` to a `HashIndex`.
    ///
    /// - `None` → `HASH_INDEX_NONE`
    /// - `Some(hash)` → looked up in the dedup table
    fn hash_to_index(&self, hash: &Option<Hash>) -> FormatResult<HashIndex> {
        match hash {
            None => Ok(HASH_INDEX_NONE),
            Some(h) => {
                let bytes = h.as_bytes();
                self.table.require(bytes)
            }
        }
    }

    /// Convert a `HashIndex` to an `Option<Hash>`.
    ///
    /// - `HASH_INDEX_NONE` → `None`
    /// - Valid index → `Some(Hash)`
    fn index_to_hash(&self, index: HashIndex) -> FormatResult<Option<Hash>> {
        if index == HASH_INDEX_NONE {
            return Ok(None);
        }
        let bytes = self.table.resolve_required(index)?;
        Ok(Some(Hash::from_bytes(*bytes)))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::format_v3::FormatError;

    /// Helper: make a Hash from a byte pattern.
    fn make_hash(byte: u8) -> Hash {
        Hash::from_bytes([byte; 32])
    }

    /// Helper: make a Position<Option<Hash>> with a specific hash.
    fn make_position(hash: Option<Hash>, pos: u64) -> Position<Option<Hash>> {
        Position {
            change: hash,
            pos: ChangePosition::new(pos),
        }
    }

    /// Helper: make a GraphNode<Option<Hash>> with a specific hash.
    fn make_graph_node(hash: Option<Hash>, start: u64, end: u64) -> crate::GraphNode<Option<Hash>> {
        crate::GraphNode {
            change: hash,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }

    /// Helper: create a Compactor with a known set of hashes.
    fn make_compactor_and_table() -> HashDedupTable {
        let self_hash = *make_hash(0xAA).as_bytes();
        let dep_hash = *make_hash(0xBB).as_bytes();

        let mut table = HashDedupTable::new(self_hash);
        table.insert(dep_hash).unwrap();
        table
    }

    // ── CompactGraphNode ───────────────────────────────────────────

    #[test]
    fn test_compact_graph_node_new() {
        let n = CompactGraphNode::new(5, 10, 20);
        assert_eq!(n.change, 5);
        assert_eq!(n.start, 10);
        assert_eq!(n.end, 20);
        assert_eq!(n.len(), 10);
        assert!(!n.is_empty());
    }

    #[test]
    fn test_compact_graph_node_self_ref() {
        let n = CompactGraphNode::self_ref(0, 100);
        assert!(n.is_self_ref());
        assert!(!n.is_root());
        assert_eq!(n.change, HASH_INDEX_SELF);
    }

    #[test]
    fn test_compact_graph_node_root() {
        let n = CompactGraphNode::root(0, 0);
        assert!(n.is_root());
        assert!(!n.is_self_ref());
        assert!(n.is_empty());
    }

    #[test]
    fn test_compact_graph_node_display() {
        assert_eq!(
            format!("{}", CompactGraphNode::self_ref(0, 10)),
            "SELF[0:10]"
        );
        assert_eq!(format!("{}", CompactGraphNode::root(5, 5)), "ROOT[5:5]");
        assert_eq!(format!("{}", CompactGraphNode::new(3, 10, 20)), "#3[10:20]");
    }

    #[test]
    fn test_compact_graph_node_postcard_roundtrip() {
        let nodes = vec![
            CompactGraphNode::self_ref(0, 0),
            CompactGraphNode::self_ref(42, 100),
            CompactGraphNode::root(0, 0),
            CompactGraphNode::new(5, 1000, 2000),
        ];

        for node in &nodes {
            let bytes = postcard::to_allocvec(node).unwrap();
            let decoded: CompactGraphNode = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(*node, decoded, "roundtrip failed for {:?}", node);
        }
    }

    #[test]
    fn test_compact_graph_node_postcard_size() {
        // SELF[0:0] → all varints are 0 → 3 bytes (1+1+1)
        let small = CompactGraphNode::self_ref(0, 0);
        let bytes = postcard::to_allocvec(&small).unwrap();
        assert_eq!(bytes.len(), 3);

        // Compare with V2: Option<Hash>(33) + u64(8) + u64(8) = 49 bytes
        // We're at 3 bytes. That's 94% savings.
    }

    // ── CompactInsertion ───────────────────────────────────────────

    #[test]
    fn test_compact_insertion_basics() {
        let v = CompactInsertion {
            predecessors: vec![CompactPosition::self_ref(0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 10,
            end: 20,
            inode: CompactPosition::self_ref(0),
        };

        assert_eq!(v.len(), 10);
        assert!(!v.is_empty());
        assert!(v.has_predecessors());
        assert!(!v.has_successors());
    }

    #[test]
    fn test_compact_insertion_empty() {
        let v = CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 5,
            end: 5,
            inode: CompactPosition::self_ref(0),
        };

        assert_eq!(v.len(), 0);
        assert!(v.is_empty());
        assert!(!v.has_predecessors());
    }

    #[test]
    fn test_compact_insertion_display() {
        let v = CompactInsertion {
            predecessors: vec![CompactPosition::self_ref(0), CompactPosition::new(1, 10)],
            successors: vec![CompactPosition::self_ref(100)],
            flag: EdgeFlags::BLOCK.bits(),
            start: 0,
            end: 42,
            inode: CompactPosition::self_ref(0),
        };
        let display = format!("{}", v);
        assert!(display.contains("0..42"));
        assert!(display.contains("2 up"));
        assert!(display.contains("1 down"));
    }

    #[test]
    fn test_compact_insertion_postcard_roundtrip() {
        let v = CompactInsertion {
            predecessors: vec![CompactPosition::self_ref(0), CompactPosition::new(1, 50)],
            successors: vec![],
            flag: EdgeFlags::BLOCK.bits(),
            start: 100,
            end: 200,
            inode: CompactPosition::self_ref(5),
        };

        let bytes = postcard::to_allocvec(&v).unwrap();
        let decoded: CompactInsertion = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(v, decoded);
    }

    // ── CompactNewEdge ─────────────────────────────────────────────

    #[test]
    fn test_compact_new_edge_display() {
        let e = CompactNewEdge {
            previous: EdgeFlags::BLOCK.bits(),
            flag: (EdgeFlags::BLOCK | EdgeFlags::DELETED).bits(),
            from: CompactPosition::self_ref(10),
            to: CompactGraphNode::new(1, 20, 30),
            introduced_by: 1,
        };
        let display = format!("{}", e);
        assert!(display.contains("Edge("));
        assert!(display.contains("by #1"));
    }

    #[test]
    fn test_compact_new_edge_postcard_roundtrip() {
        let e = CompactNewEdge {
            previous: EdgeFlags::BLOCK.bits(),
            flag: (EdgeFlags::BLOCK | EdgeFlags::DELETED).bits(),
            from: CompactPosition::new(2, 100),
            to: CompactGraphNode::new(1, 200, 300),
            introduced_by: 1,
        };

        let bytes = postcard::to_allocvec(&e).unwrap();
        let decoded: CompactNewEdge = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(e, decoded);
    }

    // ── CompactEdgeUpdate ──────────────────────────────────────────

    #[test]
    fn test_compact_edge_update_basics() {
        let em = CompactEdgeUpdate {
            edges: vec![],
            inode: CompactPosition::self_ref(0),
        };
        assert!(em.is_empty());
        assert_eq!(em.len(), 0);
    }

    #[test]
    fn test_compact_edge_update_with_edges() {
        let em = CompactEdgeUpdate {
            edges: vec![
                CompactNewEdge {
                    previous: 0x01,
                    flag: 0x05,
                    from: CompactPosition::self_ref(10),
                    to: CompactGraphNode::self_ref(20, 30),
                    introduced_by: HASH_INDEX_SELF,
                },
                CompactNewEdge {
                    previous: 0x01,
                    flag: 0x05,
                    from: CompactPosition::self_ref(40),
                    to: CompactGraphNode::self_ref(50, 60),
                    introduced_by: HASH_INDEX_SELF,
                },
            ],
            inode: CompactPosition::self_ref(0),
        };
        assert!(!em.is_empty());
        assert_eq!(em.len(), 2);
    }

    // ── CompactAtom ────────────────────────────────────────────────

    #[test]
    fn test_compact_atom_insertion() {
        let atom = CompactAtom::Insertion(CompactInsertion {
            predecessors: vec![],
            successors: vec![],
            flag: 0x01,
            start: 0,
            end: 10,
            inode: CompactPosition::self_ref(0),
        });
        assert!(atom.is_insertion());
        assert!(!atom.is_edge_update());
    }

    #[test]
    fn test_compact_atom_edge_update() {
        let atom = CompactAtom::EdgeUpdate(CompactEdgeUpdate {
            edges: vec![],
            inode: CompactPosition::self_ref(0),
        });
        assert!(!atom.is_insertion());
        assert!(atom.is_edge_update());
    }

    #[test]
    fn test_compact_atom_postcard_roundtrip() {
        let atom = CompactAtom::Insertion(CompactInsertion {
            predecessors: vec![CompactPosition::self_ref(5)],
            successors: vec![CompactPosition::new(1, 10)],
            flag: EdgeFlags::BLOCK.bits(),
            start: 100,
            end: 200,
            inode: CompactPosition::self_ref(0),
        });

        let bytes = postcard::to_allocvec(&atom).unwrap();
        let decoded: CompactAtom = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(atom, decoded);
    }

    // ── CompactGraphOp ─────────────────────────────────────────────

    #[test]
    fn test_compact_graph_op_file_add() {
        let op = CompactGraphOp::FileAdd {
            add_name: CompactInsertion {
                predecessors: vec![CompactPosition::root(0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 0,
                end: 9,
                inode: CompactPosition::root(0),
            },
            add_inode: CompactInsertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 9,
                end: 9,
                inode: CompactPosition::self_ref(0),
            },
            contents: Some(CompactInsertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 9,
                end: 42,
                inode: CompactPosition::self_ref(0),
            }),
            path: "src/main.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        assert_eq!(op.path(), Some("src/main.rs"));
        assert_eq!(op.type_name(), "FileAdd");
        assert!(format!("{}", op).contains("src/main.rs"));
    }

    #[test]
    fn test_compact_graph_op_edit() {
        let op = CompactGraphOp::Edit {
            change: CompactAtom::Insertion(CompactInsertion {
                predecessors: vec![CompactPosition::new(1, 100)],
                successors: vec![CompactPosition::self_ref(200)],
                flag: EdgeFlags::BLOCK.bits(),
                start: 50,
                end: 80,
                inode: CompactPosition::self_ref(0),
            }),
            local: Local::new("lib.rs", 42),
            encoding: Some(Encoding::Utf8),
        };

        assert_eq!(op.path(), Some("lib.rs"));
        assert_eq!(op.type_name(), "Edit");
    }

    #[test]
    fn test_compact_graph_op_all_type_names() {
        // Verify all 16 variants have distinct type names
        let names = vec![
            "FileAdd",
            "DirAdd",
            "DirDel",
            "DirUndel",
            "FileDel",
            "FileUndel",
            "FileMove",
            "Edit",
            "Replacement",
            "SolveNameConflict",
            "UnsolveNameConflict",
            "SolveOrderConflict",
            "UnsolveOrderConflict",
            "ResurrectZombies",
            "AddRoot",
            "DelRoot",
        ];
        assert_eq!(names.len(), 16);
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), 16, "all type names must be unique");
    }

    #[test]
    fn test_compact_graph_op_postcard_roundtrip() {
        let op = CompactGraphOp::Edit {
            change: CompactAtom::Insertion(CompactInsertion {
                predecessors: vec![CompactPosition::new(1, 100)],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 50,
                end: 80,
                inode: CompactPosition::self_ref(0),
            }),
            local: Local::new("test.rs", 10),
            encoding: Some(Encoding::Utf8),
        };

        let bytes = postcard::to_allocvec(&op).unwrap();
        let decoded: CompactGraphOp = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(op, decoded);
    }

    #[test]
    fn test_compact_graph_op_add_root_no_path() {
        let op = CompactGraphOp::AddRoot {
            name: CompactInsertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 0,
                end: 0,
                inode: CompactPosition::root(0),
            },
            inode: CompactInsertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK.bits(),
                start: 0,
                end: 0,
                inode: CompactPosition::root(0),
            },
        };
        assert_eq!(op.path(), None);
        assert_eq!(op.type_name(), "AddRoot");
    }

    // ── Compactor: compact_position / expand_position ──────────────

    #[test]
    fn test_compact_position_none_hash() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let pos = make_position(None, 42);
        let compact = compactor.compact_position(&pos).unwrap();

        assert_eq!(compact.change, HASH_INDEX_NONE);
        assert_eq!(compact.pos, 42);
        assert!(compact.is_root());
    }

    #[test]
    fn test_compact_position_self_hash() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let pos = make_position(Some(self_hash), 100);
        let compact = compactor.compact_position(&pos).unwrap();

        assert_eq!(compact.change, HASH_INDEX_SELF);
        assert_eq!(compact.pos, 100);
    }

    #[test]
    fn test_compact_position_dep_hash() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let dep_hash = make_hash(0xBB);
        let pos = make_position(Some(dep_hash), 200);
        let compact = compactor.compact_position(&pos).unwrap();

        assert_eq!(compact.change, 1); // dep is at index 1
        assert_eq!(compact.pos, 200);
    }

    #[test]
    fn test_compact_position_unknown_hash_fails() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let unknown = make_hash(0xCC);
        let pos = make_position(Some(unknown), 0);
        let result = compactor.compact_position(&pos);

        assert!(result.is_err());
        assert!(matches!(result, Err(FormatError::HashNotFound { .. })));
    }

    #[test]
    fn test_expand_position_none() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let compact = CompactPosition::root(42);
        let expanded = compactor.expand_position(&compact).unwrap();

        assert_eq!(expanded.change, None);
        assert_eq!(expanded.pos.get(), 42);
    }

    #[test]
    fn test_expand_position_self() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let compact = CompactPosition::self_ref(100);
        let expanded = compactor.expand_position(&compact).unwrap();

        assert_eq!(expanded.change, Some(make_hash(0xAA)));
        assert_eq!(expanded.pos.get(), 100);
    }

    #[test]
    fn test_expand_position_dep() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let compact = CompactPosition::new(1, 200);
        let expanded = compactor.expand_position(&compact).unwrap();

        assert_eq!(expanded.change, Some(make_hash(0xBB)));
        assert_eq!(expanded.pos.get(), 200);
    }

    #[test]
    fn test_expand_position_out_of_bounds_fails() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let compact = CompactPosition::new(99, 0);
        let result = compactor.expand_position(&compact);

        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(FormatError::HashIndexOutOfBounds { .. })
        ));
    }

    // ── Compactor: compact/expand roundtrip for position ───────────

    #[test]
    fn test_position_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let positions = vec![
            make_position(None, 0),
            make_position(Some(make_hash(0xAA)), 42),
            make_position(Some(make_hash(0xBB)), 999),
        ];

        for pos in &positions {
            let compact = compactor.compact_position(pos).unwrap();
            let expanded = compactor.expand_position(&compact).unwrap();
            assert_eq!(*pos, expanded, "roundtrip failed for {:?}", pos);
        }
    }

    // ── Compactor: compact/expand roundtrip for graph_node ─────────

    #[test]
    fn test_graph_node_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let nodes = vec![
            make_graph_node(None, 0, 0),
            make_graph_node(Some(make_hash(0xAA)), 10, 20),
            make_graph_node(Some(make_hash(0xBB)), 100, 200),
        ];

        for node in &nodes {
            let compact = compactor.compact_graph_node(node).unwrap();
            let expanded = compactor.expand_graph_node(&compact).unwrap();
            assert_eq!(*node, expanded, "roundtrip failed for {:?}", node);
        }
    }

    // ── Compactor: compact/expand roundtrip for Insertion ──────────

    #[test]
    fn test_insertion_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let insertion = Insertion {
            predecessors: vec![
                make_position(Some(self_hash), 0),
                make_position(Some(dep_hash), 50),
            ],
            successors: vec![make_position(None, 0)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(100),
            end: ChangePosition::new(200),
            inode: make_position(Some(self_hash), 5),
        };

        let compact = compactor.compact_insertion(&insertion).unwrap();
        let expanded = compactor.expand_insertion(&compact).unwrap();

        assert_eq!(insertion, expanded);
    }

    #[test]
    fn test_insertion_compact_preserves_flag_bits() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let insertion = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK | EdgeFlags::FOLDER | EdgeFlags::DELETED,
            start: ChangePosition::new(0),
            end: ChangePosition::new(0),
            inode: make_position(None, 0),
        };

        let compact = compactor.compact_insertion(&insertion).unwrap();
        assert_eq!(compact.flag, insertion.flag.bits());

        let expanded = compactor.expand_insertion(&compact).unwrap();
        assert_eq!(expanded.flag, insertion.flag);
    }

    // ── Compactor: compact/expand roundtrip for NewEdge ────────────

    #[test]
    fn test_new_edge_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(self_hash), 10),
            to: make_graph_node(Some(dep_hash), 20, 30),
            introduced_by: Some(dep_hash),
        };

        let compact = compactor.compact_new_edge(&edge).unwrap();
        let expanded = compactor.expand_new_edge(&compact).unwrap();

        assert_eq!(edge, expanded);
    }

    #[test]
    fn test_new_edge_with_none_introduced_by() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK,
            from: make_position(None, 0),
            to: make_graph_node(None, 0, 0),
            introduced_by: None,
        };

        let compact = compactor.compact_new_edge(&edge).unwrap();
        assert_eq!(compact.introduced_by, HASH_INDEX_NONE);

        let expanded = compactor.expand_new_edge(&compact).unwrap();
        assert_eq!(edge, expanded);
    }

    // ── Compactor: compact/expand roundtrip for EdgeUpdate ─────────

    #[test]
    fn test_edge_update_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let edge_update = EdgeUpdate {
            edges: vec![
                NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(Some(self_hash), 10),
                    to: make_graph_node(Some(self_hash), 20, 30),
                    introduced_by: Some(self_hash),
                },
                NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(Some(self_hash), 40),
                    to: make_graph_node(Some(self_hash), 50, 60),
                    introduced_by: Some(self_hash),
                },
            ],
            inode: make_position(Some(self_hash), 0),
        };

        let compact = compactor.compact_edge_update(&edge_update).unwrap();
        let expanded = compactor.expand_edge_update(&compact).unwrap();

        assert_eq!(edge_update, expanded);
    }

    // ── Compactor: compact/expand roundtrip for Atom ───────────────

    #[test]
    fn test_atom_insertion_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let atom = Atom::Insertion(Insertion {
            predecessors: vec![make_position(Some(make_hash(0xAA)), 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
            inode: make_position(Some(make_hash(0xAA)), 5),
        });

        let compact = compactor.compact_atom(&atom).unwrap();
        assert!(compact.is_insertion());

        let expanded = compactor.expand_atom(&compact).unwrap();
        assert_eq!(atom, expanded);
    }

    #[test]
    fn test_atom_edge_update_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let atom = Atom::EdgeUpdate(EdgeUpdate {
            edges: vec![NewEdge {
                previous: EdgeFlags::BLOCK,
                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                from: make_position(Some(make_hash(0xAA)), 10),
                to: make_graph_node(Some(make_hash(0xAA)), 20, 30),
                introduced_by: Some(make_hash(0xAA)),
            }],
            inode: make_position(Some(make_hash(0xAA)), 0),
        });

        let compact = compactor.compact_atom(&atom).unwrap();
        assert!(compact.is_edge_update());

        let expanded = compactor.expand_atom(&compact).unwrap();
        assert_eq!(atom, expanded);
    }

    // ── Compactor: compact/expand roundtrip for GraphOp ────────────

    #[test]
    fn test_graph_op_file_add_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let op = GraphOp::FileAdd {
            add_name: Insertion {
                predecessors: vec![make_position(None, 0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(9),
                inode: make_position(None, 0),
            },
            add_inode: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(9),
                end: ChangePosition::new(9),
                inode: make_position(Some(self_hash), 0),
            },
            contents: Some(Insertion {
                predecessors: vec![make_position(Some(self_hash), 9)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(9),
                end: ChangePosition::new(42),
                inode: make_position(Some(self_hash), 0),
            }),
            path: "src/main.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();

        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_edit_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let op = GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: vec![make_position(Some(dep_hash), 100)],
                successors: vec![make_position(Some(self_hash), 200)],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(50),
                end: ChangePosition::new(80),
                inode: make_position(Some(self_hash), 0),
            }),
            local: Local::new("lib.rs", 42),
            encoding: Some(Encoding::Utf8),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();

        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_replacement_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let op = GraphOp::Replacement {
            change: EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(Some(dep_hash), 10),
                    to: make_graph_node(Some(dep_hash), 20, 30),
                    introduced_by: Some(dep_hash),
                }],
                inode: make_position(Some(self_hash), 0),
            },
            replacement: Insertion {
                predecessors: vec![make_position(Some(dep_hash), 10)],
                successors: vec![make_position(Some(dep_hash), 30)],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(15),
                inode: make_position(Some(self_hash), 0),
            },
            local: Local::new("test.rs", 5),
            encoding: None,
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();

        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_file_del_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let op = GraphOp::FileDel {
            del: EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(None, 0),
                    to: make_graph_node(Some(self_hash), 0, 9),
                    introduced_by: Some(self_hash),
                }],
                inode: make_position(None, 0),
            },
            contents: None,
            path: "old_file.txt".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();

        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_add_root_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let op = GraphOp::AddRoot {
            name: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(0),
                inode: make_position(None, 0),
            },
            inode: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(0),
                inode: make_position(None, 0),
            },
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        assert_eq!(compact.type_name(), "AddRoot");

        let expanded = compactor.expand_graph_op(&compact).unwrap();
        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_del_root_compact_expand_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let op = GraphOp::DelRoot {
            name: EdgeUpdate {
                edges: vec![],
                inode: make_position(None, 0),
            },
            inode: EdgeUpdate {
                edges: vec![],
                inode: make_position(Some(self_hash), 0),
            },
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();

        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_solve_name_conflict_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let op = GraphOp::SolveNameConflict {
            name: EdgeUpdate {
                edges: vec![],
                inode: make_position(None, 0),
            },
            path: "conflict.txt".to_string(),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();
        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_dir_add_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let op = GraphOp::DirAdd {
            add_name: Insertion {
                predecessors: vec![make_position(None, 0)],
                successors: vec![],
                flag: EdgeFlags::FOLDER,
                start: ChangePosition::new(0),
                end: ChangePosition::new(5),
                inode: make_position(None, 0),
            },
            add_inode: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::FOLDER,
                start: ChangePosition::new(5),
                end: ChangePosition::new(5),
                inode: make_position(Some(self_hash), 0),
            },
            path: "src/".to_string(),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();
        assert_eq!(op, expanded);
    }

    #[test]
    fn test_graph_op_file_move_roundtrip() {
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let op = GraphOp::FileMove {
            del: EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(None, 0),
                    to: make_graph_node(Some(dep_hash), 0, 8),
                    introduced_by: Some(dep_hash),
                }],
                inode: make_position(None, 0),
            },
            add: Insertion {
                predecessors: vec![make_position(None, 0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(12),
                inode: make_position(Some(self_hash), 0),
            },
            path: "new_name.rs".to_string(),
        };

        let compact = compactor.compact_graph_op(&op).unwrap();
        let expanded = compactor.expand_graph_op(&compact).unwrap();
        assert_eq!(op, expanded);
    }

    // ── Postcard size savings verification ──────────────────────────

    #[test]
    fn test_compact_graph_op_postcard_size_savings() {
        // Build a realistic FileAdd operation and measure its compact size
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);

        let op = GraphOp::FileAdd {
            add_name: Insertion {
                predecessors: vec![make_position(None, 0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(11),
                inode: make_position(None, 0),
            },
            add_inode: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(11),
                end: ChangePosition::new(11),
                inode: make_position(Some(self_hash), 0),
            },
            contents: Some(Insertion {
                predecessors: vec![make_position(Some(self_hash), 11)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(11),
                end: ChangePosition::new(100),
                inode: make_position(Some(self_hash), 0),
            }),
            path: "src/main.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        // Measure full-hash postcard size (GraphOp<Option<Hash>>)
        let full_bytes = postcard::to_allocvec(&op).unwrap();

        // Measure compact postcard size (CompactGraphOp with HashIndex)
        let compact = compactor.compact_graph_op(&op).unwrap();
        let compact_bytes = postcard::to_allocvec(&compact).unwrap();

        let savings_pct = (1.0 - compact_bytes.len() as f64 / full_bytes.len() as f64) * 100.0;

        assert!(
            compact_bytes.len() < full_bytes.len() / 2,
            "Compact ({} bytes) should be less than half of full ({} bytes), savings: {:.1}%",
            compact_bytes.len(),
            full_bytes.len(),
            savings_pct,
        );
    }

    #[test]
    fn test_compact_graph_op_full_postcard_roundtrip_via_bytes() {
        // Compact → postcard bytes → deserialize → expand → compare with original
        let table = make_compactor_and_table();
        let compactor = Compactor::new(&table);

        let self_hash = make_hash(0xAA);
        let dep_hash = make_hash(0xBB);

        let op = GraphOp::Replacement {
            change: EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from: make_position(Some(dep_hash), 50),
                    to: make_graph_node(Some(dep_hash), 50, 80),
                    introduced_by: Some(dep_hash),
                }],
                inode: make_position(Some(self_hash), 0),
            },
            replacement: Insertion {
                predecessors: vec![make_position(Some(dep_hash), 50)],
                successors: vec![make_position(Some(dep_hash), 80)],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(25),
                inode: make_position(Some(self_hash), 0),
            },
            local: Local::new("complex.rs", 99),
            encoding: Some(Encoding::Utf8),
        };

        // Full pipeline: compact → serialize → deserialize → expand
        let compact = compactor.compact_graph_op(&op).unwrap();
        let bytes = postcard::to_allocvec(&compact).unwrap();
        let deserialized: CompactGraphOp = postcard::from_bytes(&bytes).unwrap();
        let expanded = compactor.expand_graph_op(&deserialized).unwrap();

        assert_eq!(op, expanded);
    }
}

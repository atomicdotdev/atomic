//! The [`Compactor`] converts between full-hash graph types and compact
//! index-based types using a [`HashDedupTable`].

use super::super::error::FormatResult;
use super::super::hash_table::HashDedupTable;
use super::super::types::{CompactPosition, HashIndex, HASH_INDEX_NONE};
use super::types::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactInsertion, CompactNewEdge,
};
use crate::change::atom::{Atom, EdgeUpdate, Insertion, NewEdge};
use crate::types::{ChangePosition, EdgeFlags};
use crate::Hash;
use crate::Position;

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
/// - `compact_*` methods return [`FormatError::HashNotFound`](super::super::error::FormatError::HashNotFound)
///   if a hash isn't in the dedup table.
/// - `expand_*` methods return [`FormatError::HashIndexOutOfBounds`](super::super::error::FormatError::HashIndexOutOfBounds)
///   if an index exceeds the table size.
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
    /// Returns [`FormatError::HashNotFound`](super::super::error::FormatError::HashNotFound)
    /// if the hash isn't in the dedup table.
    pub fn compact_position(&self, pos: &Position<Option<Hash>>) -> FormatResult<CompactPosition> {
        let change = self.hash_to_index(&pos.change)?;
        Ok(CompactPosition::new(change, pos.pos.get() as u32))
    }

    /// Convert a `GraphNode<Option<Hash>>` to a [`CompactGraphNode`].
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::HashNotFound`](super::super::error::FormatError::HashNotFound)
    /// if the hash isn't in the dedup table.
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

    // ── Expand: Compact → Full ─────────────────────────────────────

    /// Convert a [`CompactPosition`] to a `Position<Option<Hash>>`.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::HashIndexOutOfBounds`](super::super::error::FormatError::HashIndexOutOfBounds)
    /// if the index is invalid.
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

    // ── Internal Helpers ───────────────────────────────────────────

    /// Convert an `Option<Hash>` to a `HashIndex`.
    ///
    /// - `None` → `HASH_INDEX_NONE`
    /// - `Some(hash)` → looked up in the dedup table
    pub(super) fn hash_to_index(&self, hash: &Option<Hash>) -> FormatResult<HashIndex> {
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
    pub(super) fn index_to_hash(&self, index: HashIndex) -> FormatResult<Option<Hash>> {
        if index == HASH_INDEX_NONE {
            return Ok(None);
        }
        let bytes = self.table.resolve_required(index)?;
        Ok(Some(Hash::from_bytes(*bytes)))
    }
}

//! Compact graph types for V3 serialization.
//!
//! This module defines space-efficient versions of the core graph types that
//! use [`HashIndex`](super::types::HashIndex) references (2 bytes) instead of
//! full 32-byte hashes. These types exist **solely for serialization** — they
//! are never used in the in-memory graph. Conversion happens at the
//! read/write boundary:
//!
//! ```text
//! Recording:  GraphOp<Option<Hash>> → CompactGraphOp → postcard → zstd → disk
//! Applying:   disk → zstd → postcard → CompactGraphOp → GraphOp<Option<Hash>>
//! ```
//!
//! # Why Compact Types?
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash plus a
//! 1-byte `Option` discriminant. A `GraphNode<Option<Hash>>` stores
//! 33 + 8 + 8 = 49 bytes. The compact types replace `Option<Hash>` with
//! `HashIndex` (`u16`) and `u64` positions with `u32`. Combined with
//! postcard's varint encoding:
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
//! | `Position<Option<Hash>>` | [`CompactPosition`](super::types::CompactPosition) | `Option<Hash>` → `HashIndex` |
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
//! All compact types are `Send + Sync`. [`Compactor`] borrows a
//! [`HashDedupTable`](super::hash_table::HashDedupTable) immutably and is
//! also `Send + Sync`.

pub mod compactor;
mod convert;
pub mod graph_op;
pub mod types;

#[cfg(test)]
mod tests;

// ── Re-exports ─────────────────────────────────────────────────────────

pub use compactor::Compactor;
pub use graph_op::CompactGraphOp;
pub use types::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactInsertion, CompactNewEdge,
};

//! Pristine storage layer for Atomic VCS
//!
//! The pristine is the persistent storage layer that holds the repository's
//! graph state. It stores vertices, edges, change mappings, file trees, and
//! view metadata in a transactional database.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                        Pristine Database                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
//! │  │ ID Mappings │  │    Graph    │  │        Views            │  │
//! │  │             │  │             │  │                         │  │
//! │  │ EXTERNAL    │  │ GRAPH       │  │ VIEWS                   │  │
//! │  │ INTERNAL    │  │ INODE_GRAPH │  │ VIEW_CHANGES            │  │
//! │  │ NODE_TYPES  │  │             │  │ REV_VIEW_CHANGES        │  │
//! │  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
//! │  │  File Tree  │  │Dependencies │  │         State           │  │
//! │  │             │  │             │  │                         │  │
//! │  │ TREE        │  │ DEPS        │  │ STATES                  │  │
//! │  │ REV_TREE    │  │ REV_DEPS    │  │ TAGS                    │  │
//! │  │ INODES      │  │             │  │                         │  │
//! │  │ REV_INODES  │  │             │  │                         │  │
//! │  └─────────────┘  └─────────────┘  └─────────────────────────┘  │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Views vs Branches
//!
//! Atomic uses **Views** instead of branches. This is a fundamental conceptual
//! difference from Git:
//!
//! | Concept | Git Branches | Atomic Views |
//! |---------|--------------|--------------|
//! | Nature | Fork of history | View of the graph |
//! | Data | Duplicates commits | References same changes |
//! | Merge | Combines divergent histories | Applies missing changes |
//! | Identity | Pointer to a commit | Ordered sequence + Merkle state |
//!
//! Views are perspectives of the graph - they represent which changes have been
//! applied and in what order. Multiple views can coexist, each showing a
//! different perspective on the same underlying data. When you "merge" views,
//! you're really just applying changes that one view has but the other doesn't.
//!
//! # Storage Backend
//!
//! We use [redb](https://docs.rs/redb) as the storage backend:
//!
//! - **Pure Rust**: No C dependencies, simpler builds
//! - **ACID Transactions**: Safe concurrent access with isolation
//! - **Copy-on-Write B-trees**: Efficient updates without full rewrites
//! - **Memory-mapped I/O**: Excellent read performance
//!
//! # Transaction Model
//!
//! The pristine uses a transaction-based access model:
//!
//! ```text
//! ┌──────────────┐     ┌──────────────┐
//! │   Pristine   │────▶│   ReadTxn    │  (multiple concurrent)
//! │   (handle)   │     └──────────────┘
//! │              │     ┌──────────────┐
//! │              │────▶│   WriteTxn   │  (single exclusive)
//! └──────────────┘     └──────────────┘
//! ```
//!
//! - **ReadTxn**: Read-only snapshot, multiple can run concurrently
//! - **WriteTxn**: Exclusive write access, must commit or abort
//!
//! # Usage Example
//!
//! ```ignore
//! use atomic_core::pristine::{Pristine, MutTxnT, ViewTxnT, GraphTxnT};
//! use atomic_core::types::{Hash, NodeId, GraphNode, EdgeFlags};
//!
//! // Open or create the database
//! let pristine = Pristine::open("path/to/.atomic/pristine")?;
//!
//! // Read-only access
//! {
//!     let txn = pristine.read_txn()?;
//!     let view = txn.get_view("main")?;
//!     let views = txn.list_views()?;
//! }
//!
//! // Write access
//! {
//!     let mut txn = pristine.write_txn()?;
//!
//!     // Create or open a view
//!     let mut view = txn.open_or_create_view("feature")?;
//!
//!     // Register a change
//!     let hash = Hash::of(b"change content");
//!     let change_id = txn.register_change(&hash)?;
//!
//!     // Record the change in the view
//!     txn.put_change(&mut view, change_id, &hash)?;
//!     txn.update_view(&view)?;
//!
//!     // Commit the transaction
//!     txn.commit()?;
//! }
//! ```
//!
//! # Module Organization
//!
//! - `error` - Error types (`PristineError`, `PristineResult`)
//! - [`tables`] - redb table definitions and key encoding helpers
//! - `traits` - Database trait abstractions (`GraphTxnT`, `ViewTxnT`, `TreeTxnT`, `MutTxnT`)
//! - `txn` - Transaction implementations (`Pristine`, `ReadTxn`, `WriteTxn`)
//!
//! # Table Reference
//!
//! | Table | Key | Value | Purpose |
//! |-------|-----|-------|---------|
//! | `EXTERNAL` | NodeId | Hash | Internal → external ID mapping |
//! | `INTERNAL` | Hash | NodeId | External → internal ID mapping |
//! | `NODE_TYPES` | NodeId | u8 | Type of node (change, tag) |
//! | `GRAPH` | Span | `Edge` | Main graph (multimap) |
//! | `INODE_GRAPH` | (Inode, Span) | `Edge` | File-scoped graph index |
//! | `VIEWS` | name | ViewState | View metadata |
//! | `VIEW_CHANGES` | (view_id, seq) | change_id | Change log |
//! | `REV_VIEW_CHANGES` | (view_id, change_id) | seq | Reverse change log |
//! | `TREE` | path | inode | Path → inode mapping |
//! | `REV_TREE` | inode | path | Inode → path mapping |
//! | `INODES` | inode | Position | Inode → graph position |
//! | `REV_INODES` | Position | inode | Graph position → inode |
//! | `DEPS` | change_id | `dep_id` | Dependencies (multimap) |
//! | `REV_DEPS` | dep_id | `change_id` | Reverse dependencies |
//! | `STATES` | (view_id, merkle) | seq | State → sequence lookup |
//! | `TAGS` | (view_id, seq) | merkle | Tagged states |
//!
//! # Performance Characteristics
//!
//! | Operation | Complexity | Notes |
//! |-----------|------------|-------|
//! | Get span edges | O(k) | k = number of edges |
//! | Find block | O(log n) | Binary search in B-tree |
//! | Register change | O(log n) | Two table insertions |
//! | Iterate file | O(m) | m = file size in vertices |
//! | List views | O(s) | s = number of views |
//!
//! The `INODE_GRAPH` secondary index enables O(n) file traversal where n is
//! proportional to file size, rather than O(N) where N is total graph size.

mod error;
mod inode_graph;
pub mod ontology;
pub mod tables;
mod traits;
mod txn;
pub mod vault;
pub mod view_graph;

pub use error::{PristineError, PristineResult};
pub use inode_graph::{
    InodeAdjState, InodeEdgeIter, InodeGraphOps, InodeGraphStats, InodeVertex, IntoInodeVertex,
};
pub use tables::directory_flags;
pub use tables::*;
pub use traits::{
    EmbeddingsMutTxnT, EmbeddingsTxnT, GraphTxnT, KgMutTxnT, KgTxnT, MutTxnT, TreeTxnT,
    VaultEntryMeta, VaultMutTxnT, VaultTxnT, VertexExt, ViewScope, ViewState, ViewTxnT,
};
pub use txn::{AdjIterator, Pristine, ReadTxn, WriteTxn};
pub use vault::{
    EmbeddingRecord, GoalSummary, IndexStats, IntentSummary, KgEdge, KgNode, KgQueryResponse,
    KgSubgraph, MemorySummary, SearchResult, SkillSummary, TokenCounts, VaultEntry, VaultEntryType,
    VaultManifest,
};
pub use view_graph::ViewGraph;

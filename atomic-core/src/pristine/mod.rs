//! Pristine storage layer for Atomic VCS
//!
//! The pristine is the persistent storage layer that holds the repository's
//! graph state. It stores vertices, edges, change mappings, file trees, and
//! stack (view) metadata in a transactional database.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                        Pristine Database                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
//! │  │ ID Mappings │  │    Graph    │  │        Stacks           │  │
//! │  │             │  │             │  │                         │  │
//! │  │ EXTERNAL    │  │ GRAPH       │  │ STACKS                  │  │
//! │  │ INTERNAL    │  │ INODE_GRAPH │  │ STACK_CHANGES           │  │
//! │  │ NODE_TYPES  │  │             │  │ REV_STACK_CHANGES       │  │
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
//! # Stacks vs Branches
//!
//! Atomic uses **Stacks** instead of branches. This is a fundamental conceptual
//! difference from Git:
//!
//! | Concept | Git Branches | Atomic Stacks |
//! |---------|--------------|---------------|
//! | Nature | Fork of history | View of the graph |
//! | Data | Duplicates commits | References same changes |
//! | Merge | Combines divergent histories | Applies missing changes |
//! | Identity | Pointer to a commit | Ordered sequence + Merkle state |
//!
//! Stacks are **views** of the graph - they represent which changes have been
//! applied and in what order. Multiple stacks can coexist, each showing a
//! different perspective on the same underlying data. When you "merge" stacks,
//! you're really just applying changes that one stack has but the other doesn't.
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
//! use atomic_core::pristine::{Pristine, MutTxnT, StackTxnT, GraphTxnT};
//! use atomic_core::types::{Hash, NodeId, GraphNode, EdgeFlags};
//!
//! // Open or create the database
//! let pristine = Pristine::open("path/to/.atomic/pristine")?;
//!
//! // Read-only access
//! {
//!     let txn = pristine.read_txn()?;
//!     let stack = txn.get_stack("main")?;
//!     let stacks = txn.list_stacks()?;
//! }
//!
//! // Write access
//! {
//!     let mut txn = pristine.write_txn()?;
//!
//!     // Create or open a stack
//!     let mut stack = txn.open_or_create_stack("feature")?;
//!
//!     // Register a change
//!     let hash = Hash::of(b"change content");
//!     let change_id = txn.register_change(&hash)?;
//!
//!     // Record the change in the stack
//!     txn.put_change(&mut stack, change_id, &hash)?;
//!     txn.update_stack(&stack)?;
//!
//!     // Commit the transaction
//!     txn.commit()?;
//! }
//! ```
//!
//! # Module Organization
//!
//! - [`error`] - Error types (`PristineError`, `PristineResult`)
//! - [`tables`] - redb table definitions and key encoding helpers
//! - [`traits`] - Database trait abstractions (`GraphTxnT`, `StackTxnT`, `TreeTxnT`, `MutTxnT`)
//! - [`txn`] - Transaction implementations (`Pristine`, `ReadTxn`, `WriteTxn`)
//!
//! # Table Reference
//!
//! | Table | Key | Value | Purpose |
//! |-------|-----|-------|---------|
//! | `EXTERNAL` | NodeId | Hash | Internal → external ID mapping |
//! | `INTERNAL` | Hash | NodeId | External → internal ID mapping |
//! | `NODE_TYPES` | NodeId | u8 | Type of node (change, tag) |
//! | `GRAPH` | Span | [Edge] | Main graph (multimap) |
//! | `INODE_GRAPH` | (Inode, Span) | [Edge] | File-scoped graph index |
//! | `STACKS` | name | StackState | Stack metadata |
//! | `STACK_CHANGES` | (stack_id, seq) | change_id | Change log |
//! | `REV_STACK_CHANGES` | (stack_id, change_id) | seq | Reverse change log |
//! | `TREE` | path | inode | Path → inode mapping |
//! | `REV_TREE` | inode | path | Inode → path mapping |
//! | `INODES` | inode | Position | Inode → graph position |
//! | `REV_INODES` | Position | inode | Graph position → inode |
//! | `DEPS` | change_id | [dep_id] | Dependencies (multimap) |
//! | `REV_DEPS` | dep_id | [change_id] | Reverse dependencies |
//! | `STATES` | (stack_id, merkle) | seq | State → sequence lookup |
//! | `TAGS` | (stack_id, seq) | merkle | Tagged states |
//!
//! # Performance Characteristics
//!
//! | Operation | Complexity | Notes |
//! |-----------|------------|-------|
//! | Get span edges | O(k) | k = number of edges |
//! | Find block | O(log n) | Binary search in B-tree |
//! | Register change | O(log n) | Two table insertions |
//! | Iterate file | O(m) | m = file size in vertices |
//! | List stacks | O(s) | s = number of stacks |
//!
//! The `INODE_GRAPH` secondary index enables O(n) file traversal where n is
//! proportional to file size, rather than O(N) where N is total graph size.

mod error;
mod inode_graph;
mod tables;
mod traits;
mod txn;

pub use error::{PristineError, PristineResult};
pub use inode_graph::{
    InodeAdjState, InodeEdgeIter, InodeGraphOps, InodeGraphStats, InodeVertex, IntoInodeVertex,
};
pub use tables::*;
pub use tables::directory_flags;
pub use traits::{GraphTxnT, MutTxnT, StackState, StackTxnT, TreeTxnT, VertexExt};
pub use txn::{AdjIterator, Pristine, ReadTxn, WriteTxn};

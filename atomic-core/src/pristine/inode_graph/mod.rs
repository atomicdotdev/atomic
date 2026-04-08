//! Inode-scoped graph operations for two-level B-tree optimization
//!
//! This module provides the `InodeGraphOps` trait which enables efficient
//! file-local graph traversal using a dual B-tree indexing strategy.
//!
//! # Implementation
//!
//! This module implements `InodeGraphOps` for both `ReadTxn` and `WriteTxn`,
//! enabling optimized file-local graph traversal using the `INODE_GRAPH`
//! secondary index.
//!
//! # Performance Rationale
//!
//! The standard graph storage uses `GraphNode<NodeId>` as the key, storing all
//! vertices from all files in a single B-tree. This leads to O(n × log N)
//! traversal complexity when iterating edges for a file, where N is the total
//! number of vertices across ALL files.
//!
//! By using `(Inode, GraphNode<NodeId>)` as a composite key in a secondary index:
//! - All edges for a single file are stored contiguously
//! - Cursor-based iteration within a file becomes O(m) where m is vertices in that file
//! - Cross-file queries remain possible via the primary index

mod impls;
mod types;

pub use types::{
    InodeAdjState, InodeEdgeIter, InodeGraphOps, InodeGraphStats, InodeVertex, IntoInodeVertex,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "integration_tests.rs"]
mod integration_tests;

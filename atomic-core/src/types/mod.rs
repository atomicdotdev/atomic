//! Core types for Atomic VCS
//!
//! This module defines the fundamental data structures used throughout
//! the Atomic version control system.
//!
//! # Graph Types
//!
//! The storage layer uses graph terminology:
//! - `GraphNode<H>` - A node in the repository DAG (content range within a change)
//! - `GraphEdge` - An edge connecting nodes (with flags for type/state)
//! - `SerializedGraphEdge` - Compact edge representation for storage
//!
//! # Hash Type Design
//!
//! Following the original Atomic project, we use a unified hash type:
//! - `Merkle` is the primary hash type (32-byte Blake3 hash)
//! - `Hash` is a type alias for `Merkle`
//!
//! This simplifies the codebase by having a single hash type for both
//! content addressing (identifying changes) and state tracking (channel state).

mod graph_edge;
mod graph_node;
mod hash;
mod node_id;
mod position;

pub use graph_edge::{EdgeFlags, GraphEdge, SerializedGraphEdge};
pub use graph_node::{GraphNode, IntoGraphNode};
pub use hash::{Hash, Hasher, Merkle};
pub use node_id::{ChangePosition, Inode, NodeId, L64};
pub use position::Position;

/// Base32 encoding trait for human-readable identifiers
pub trait Base32: Sized {
    /// Encode to a base32 string
    fn to_base32(&self) -> String;

    /// Decode from a base32 string
    fn from_base32(s: &[u8]) -> Option<Self>;
}

/// Result type alias for core operations
pub type CoreResult<T> = std::result::Result<T, CoreError>;

/// Core errors
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("Invalid base32 encoding")]
    InvalidBase32,

    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Invalid graph node: {0}")]
    InvalidGraphNode(String),

    #[error("Invalid edge flags: {0}")]
    InvalidEdgeFlags(u8),

    #[error("Position out of bounds: {0}")]
    PositionOutOfBounds(u64),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

//! GraphOp building from diff operations.
//!
//! This module provides functionality for converting diff operations into
//! repository hunks. Hunks are the semantic units of change in Atomic —
//! they represent operations like "insert these lines" or "delete this content".
//!
//! # Overview
//!
//! The graph_op builder bridges the gap between raw diff output and repository
//! graph operations:
//!
//! ```text
//! Diff Operations       GraphOp Builder         Repository Hunks
//! ┌────────────────┐   ┌──────────────┐    ┌─────────────────┐
//! │ DiffOp::Equal  │   │              │    │ (unchanged)     │
//! │ DiffOp::Insert │──▶│ HunkBuilder  │──▶ │ GraphOp::Edit   │
//! │ DiffOp::Delete │   │              │    │ (insert/delete) │
//! │ DiffOp::Replace│   │              │    │ GraphOp::Replace │
//! └────────────────┘   └──────────────┘    └─────────────────┘
//! ```
//!
//! # Submodules
//!
//! - [`options`] — [`HunkBuildOptions`] configuration
//! - [`pending`] — [`PendingChange`] intermediate representation
//! - [`hunk`] — [`BuiltHunk`], [`BuiltHunkKind`], [`HunkBuildResult`] output types
//! - [`builder`] — [`HunkBuilder`] for processing diff operations

pub mod builder;
pub mod hunk;
pub mod options;
pub mod pending;

#[cfg(test)]
mod tests;

// Re-export all public types at the module level to preserve the existing API.
// External code uses `crate::record::workflow::graph_op::HunkBuilder` etc.
pub use builder::HunkBuilder;
pub use hunk::{BuiltHunk, BuiltHunkKind, HunkBuildResult};
pub use options::HunkBuildOptions;
pub use pending::{PendingChange, PendingChangeKind};

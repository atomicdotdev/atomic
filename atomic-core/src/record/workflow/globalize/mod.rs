//! Globalization of local hunks to graph operations.
//!
//! This module converts local working copy changes (represented as [`BuiltHunk`])
//! into graph-compatible operations ([`GraphOp<Option<Hash>>`]) that can be applied
//! to the repository graph.
//!
//! # Overview
//!
//! "Globalization" is the process of converting local, file-centric change
//! representations into the global graph coordinate system used by Atomic.
//! This involves:
//!
//! 1. **Position Resolution**: Converting file paths and line numbers to graph
//!    positions (vertices and edges)
//! 2. **Span Creation**: Building [`Insertion`] structures that insert content
//!    into the graph with proper context
//! 3. **Edge Creation**: Building [`EdgeUpdate`] structures that mark existing
//!    content as deleted
//! 4. **Dependency Tracking**: Recording which existing changes the new change
//!    depends on
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Globalization Pipeline                             │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  BuiltHunk                 GlobalizeContext              GraphOp<Option<H>>│
//! │  ┌──────────────┐         ┌───────────────┐            ┌──────────────┐│
//! │  │ path: String │         │ txn: &T       │            │ FileAdd {    ││
//! │  │ line: u64    │  ────►  │ view: &View   │  ────►     │   add_name   ││
//! │  │ kind: Insert │         │ content_pos   │            │   add_inode  ││
//! │  │ content: ..  │         │ dependencies  │            │   contents   ││
//! │  └──────────────┘         └───────────────┘            │ }            ││
//! │                                                         └──────────────┘│
//! │                                                                         │
//! │  Position Resolution:                                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ path "src/main.rs"  ──►  inode(42)  ──►  Position(change, pos)   │  │
//! │  │ line 100            ──►  find span containing line 100         │  │
//! │  │                     ──►  predecessors / successors               │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Concepts
//!
//! ## Context
//!
//! When inserting new content, we specify **context** - the vertices that should
//! come before (`predecessors`) and after (`successors`) the new content. This
//! allows Atomic to correctly position content even when merging independent
//! changes.
//!
//! ## Inode Resolution
//!
//! Files are identified by **inodes** - stable identifiers that survive renames.
//! The globalization process resolves file paths to inodes, then inodes to graph
//! positions.
//!
//! ## Content Positions
//!
//! New content is appended to a content buffer. The `ChangePosition` values in
//! `Insertion` reference byte ranges within this buffer.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::globalize::{
//!     GlobalizeContext, GlobalizeOptions, globalize_recorded_file,
//! };
//!
//! // Set up context
//! let mut ctx = GlobalizeContext::new(&txn, content_buffer);
//!
//! // Globalize a recorded file
//! let hunks = globalize_recorded_file(&mut ctx, &recorded_file, &options)?;
//!
//! // The result contains graph-ready hunks
//! for graph_op in &hunks {
//!     change.add_hunk(graph_op.clone());
//! }
//! ```
//!
//! # Error Handling
//!
//! Globalization can fail for several reasons:
//!
//! - **Path not found**: The file path doesn't exist in the tree
//! - **Inode not found**: The inode has no graph position
//! - **Position not found**: Cannot find the span for a line number
//! - **Missing context**: Cannot determine up/down context for insertion
//!
//! See [`GlobalizeError`] for the complete list.

use std::collections::HashSet;
use std::fmt;

use crate::output::alive::{retrieve_graph, RetrieveOptions};

use thiserror::Error;

use crate::change::{Atom, EdgeUpdate, Encoding, GraphOp, Insertion, NewEdge};
use crate::pristine::{GraphTxnT, PristineError, TreeTxnT};
use crate::types::{ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position};

use super::graph_op::{BuiltHunk, BuiltHunkKind};
use super::record::RecordedFile;

mod context;
mod error;
mod file;
mod helpers;
mod hunk;
mod options;
mod pipeline;
mod resolve;
mod vertex;

pub use context::*;
pub use error::*;
pub use file::*;
pub use helpers::{extract_filename, extract_parent};
pub(crate) use helpers::{
    node_id_to_option_hash, position_to_option_hash, position_to_option_hash_resolved,
    vertex_to_option_hash,
};
pub use hunk::globalize_hunk;
pub use options::*;
pub use pipeline::globalize_recorded_file;
pub use resolve::*;
pub use vertex::*;

#[cfg(test)]
mod tests;

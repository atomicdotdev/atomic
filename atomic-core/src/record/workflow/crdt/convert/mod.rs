//! GraphOp to CRDT operation conversion.
//!
//! This module converts traditional `GraphOp` types (used in the existing change
//! representation) into CRDT operations (`TrunkOp`, `BranchOp`, `LeafOp`).
//! This enables the transition from the flat graph model to the hierarchical
//! CRDT model while maintaining semantic equivalence.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      GraphOp → CRDT Conversion Pipeline                     │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: GraphOp Types                                                      │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ GraphOp::FileAdd   → TrunkOp::Create + BranchOps + LeafOps          │  │
//! │  │ GraphOp::FileDel   → TrunkOp::Delete                                │  │
//! │  │ GraphOp::FileMove  → TrunkOp::Move                                  │  │
//! │  │ GraphOp::Edit      → BranchOp/LeafOp (insert/delete)                │  │
//! │  │ GraphOp::Replace   → BranchOp::Delete + BranchOp::Insert            │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  HunkConverter                                                          │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ • Analyzes graph_op type and content                                 │  │
//! │  │ • Generates appropriate CRDT operations                          │  │
//! │  │ • Tracks content positions for leaf ranges                       │  │
//! │  │ • Maintains ID allocation state                                  │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: ConvertedOps                                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ trunk_ops: Vec<TrunkOp>                                          │  │
//! │  │ branch_ops: Vec<BranchOp>                                        │  │
//! │  │ leaf_ops: Vec<LeafOp>                                            │  │
//! │  │ content: Vec<u8>  (accumulated content for leaves)               │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Conversion Rules
//!
//! | GraphOp Type | CRDT Operations |
//! |-----------|-----------------|
//! | `FileAdd` | `TrunkOp::Create` + content as `BranchOp::Insert` + `LeafOp::Insert` |
//! | `FileDel` | `TrunkOp::Delete` (cascades to branches/leaves) |
//! | `FileUndel` | `TrunkOp::Undelete` (restores branches/leaves) |
//! | `FileMove` | `TrunkOp::Move` (preserves content) |
//! | `Edit` (insert) | `BranchOp::Insert` with `LeafOp::Insert` for tokens |
//! | `Edit` (delete) | `BranchOp::Delete` or `LeafOp::Delete` |
//! | `Replacement` | Delete ops followed by insert ops |

mod converter;
mod types;

pub use converter::HunkConverter;
pub use types::{ConversionOptions, ConversionStats, ConvertError, ConvertResult, ConvertedOps};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

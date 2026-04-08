//! Line-level diff analysis for CRDT Branch operations.
//!
//! This module converts line-level diff operations into CRDT `BranchOp`
//! operations. It analyzes differences between old and new content at
//! the line level to determine which lines were inserted, deleted, or
//! modified.
//!
//! # Key Types
//!
//! - [`LineAnalyzer`]: Main entry point for analyzing line differences
//! - [`LineAnalysis`]: Result of analysis containing all line changes
//! - [`LineChange`]: A single change (equal, insert, delete, or modify)
//! - [`LineChangeKind`]: Classification of the change type
//! - [`AnalysisOptions`]: Configuration for analysis behavior
//!
//! # Integration with CRDT Model
//!
//! The analysis results map directly to CRDT operations:
//!
//! - `LineChange::Insert` → `BranchOp::Insert`
//! - `LineChange::Delete` → `BranchOp::Delete`
//! - `LineChange::Modify` → `BranchOp::Delete` + `BranchOp::Insert` (or token-level ops)
//! - `LineChange::Equal` → No operation (line unchanged)

mod analyzer;
mod types;

pub use analyzer::{AnalysisStats, LineAnalysis, LineAnalyzer};
pub use types::{AnalysisOptions, LineChange, LineChangeKind};

/// Alias for LineAnalysis for API compatibility.
pub type AnalysisResult = LineAnalysis;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

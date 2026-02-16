//! Semantic diff with token-level granularity.
//!
//! This module provides the core functionality for diffing files at multiple
//! levels of granularity:
//!
//! 1. **Line-level**: Which lines changed (added, deleted, modified)
//! 2. **Token-level**: Within modified lines, which tokens changed
//!
//! This two-level approach is essential for code reviews where you want to
//! see exactly what changed - not just that a line changed, but specifically
//! which words/tokens within that line were modified.
//!
//! # Visual Pattern
//!
//! The semantic diff enables the modern code review display pattern:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │ - const result = calculateSum(a, b);        <- light red background      │
//! │ + const result = calculateSum(a, b, c);     <- light green background    │
//! │                                   ^^^^      <- dark green: ", c" added   │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::semantic::{semantic_diff, LineChange};
//!
//! let old = b"fn main() {\n    let x = 1;\n}\n";
//! let new = b"fn main() {\n    let x = 42;\n}\n";
//!
//! let diff = semantic_diff(old, new);
//!
//! assert!(diff.has_changes());
//! for change in diff.changes() {
//!     match change {
//!         LineChange::Modified { old_line_num, new_line_num, before, after, token_changes } => {
//!             println!("Line {} -> {} modified:", old_line_num, new_line_num);
//!             for tc in token_changes {
//!                 println!("  {:?}", tc);
//!             }
//!         }
//!         LineChange::Added { line_num, line, .. } => {
//!             println!("Line {} added: {:?}", line_num, line.content_str());
//!         }
//!         LineChange::Deleted { line_num, line, .. } => {
//!             println!("Line {} deleted: {:?}", line_num, line.content_str());
//!         }
//!     }
//! }
//! ```
//!
//! # Integration with CRDT
//!
//! The semantic diff results can be used to generate CRDT operations:
//!
//! - `LineChange::Added` → `BranchOp::Insert` with `LeafOp::Insert` for tokens
//! - `LineChange::Deleted` → `BranchOp::Delete` with original content
//! - `LineChange::Modified` → Combination of token-level operations

use super::line::Line;
use super::ops::DiffOp;
use super::token::{Token, TokenKind, Tokenizer, TokenizerConfig};
use super::word::{word_diff_with_config, WordDiffConfig, WordDiffOp, WordDiffResult};
use super::{diff, Algorithm};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

mod config;
mod diff_impl;
mod types;

pub use config::*;
pub use diff_impl::*;
pub use types::*;

#[cfg(test)]
mod tests;

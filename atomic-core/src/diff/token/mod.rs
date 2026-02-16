//! Token representation for word-level diffing.
//!
//! This module provides token-level granularity for diff operations,
//! enabling CRDT-style word/character comparison for code reviews.
//! Tokens are the atomic units that allow us to show exactly what
//! changed within a line, not just that a line changed.
//!
//! # Token Types
//!
//! Tokens are classified into categories that help with:
//! - Semantic diff display (highlighting meaningful changes)
//! - Language-aware comparison (operators vs identifiers)
//! - Whitespace handling (can be optionally ignored)
//!
//! # Design Philosophy
//!
//! Unlike line-level diffs, token-level diffs can show exactly what
//! changed within a line. For code reviews, this means:
//!
//! - Single character changes are clearly highlighted
//! - Added/removed function arguments are visible
//! - Renamed variables show the exact change
//! - The diff display uses light background for the line and dark
//!   highlighting for the specific tokens that changed
//!
//! # Display Pattern
//!
//! The visual pattern this enables:
//!
//! ```text
//! - const result = calculateSum(a, b);        <- light red background
//!                                  ^^         <- no dark highlight (unchanged)
//! + const result = calculateSum(a, b, c);     <- light green background
//!                                   ^^^^      <- dark green: ", c" added
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::token::{Token, Tokenizer, TokenKind};
//!
//! let line = b"const result = calculateSum(a, b);";
//! let tokens: Vec<Token> = Tokenizer::new(line).collect();
//!
//! assert!(tokens.len() > 0);
//! assert_eq!(tokens[0].kind(), TokenKind::Word);
//! assert_eq!(tokens[0].as_str(), "const");
//! ```
//!
//! # Integration with Graph Model
//!
//! While the current implementation is for display purposes (code review),
//! the token concept could be extended to the graph model for true CRDT
//! semantics where each token has a unique span identity, enabling:
//!
//! - Per-token AI attribution
//! - Fine-grained merge conflict resolution
//! - Character-level blame

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

mod config;
mod kind;
mod token_type;
mod tokenizer;

pub use config::*;
pub use kind::*;
pub use token_type::*;
pub use tokenizer::*;

#[cfg(test)]
mod tests;

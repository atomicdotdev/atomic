//! The `change` command for viewing details about a specific change.
//!
//! This module implements the `atomic change` command, which displays detailed
//! information about a specific change (patch) in the repository. Changes can
//! be identified by their full hash, a hash prefix, or a sequence number.
//!
//! # Usage
//!
//! ```text
//! atomic change [OPTIONS] [HASH_OR_SEQ]
//!
//! Arguments:
//!   [HASH_OR_SEQ]  Change identifier (hash, hash prefix, or sequence number)
//!                  If omitted, shows the most recent change
//!
//! Options:
//!       --view <NAME>      View to use for sequence lookup (default: current)
//!   -f, --format <FORMAT>  Output format (default, short, json)
//!       --show-deps        Show dependency details
//!       --show-hunks       Show graph_op details
//!   -h, --help             Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Default Format
//!
//! The default format provides comprehensive information about the change:
//!
//! ```text
//! change ABC123456789012345678901234567890123456789012345678901
//! Author: Alice <alice@example.com>
//! Date:   Mon Jan 15 10:30:45 2024 -0500
//!
//!     Add authentication module
//!
//!     This implements JWT-based authentication for the API.
//!     Includes token generation and validation.
//!
//! Dependencies: 2
//!   DEF98765... - Initial project setup
//!   GHI11111... - Add user model
//!
//! Files changed: 3
//!   + src/auth/mod.rs
//!   + src/auth/jwt.rs
//!   ~ src/lib.rs
//! ```
//!
//! ## Short Format (-f short)
//!
//! Compact single-line format:
//!
//! ```text
//! ABC12345 2024-01-15 Alice Add authentication module
//! ```
//!
//! ## JSON Format (-f json)
//!
//! Machine-readable JSON output:
//!
//! ```text
//! {
//!   "hash": "ABC123...",
//!   "message": "Add authentication module",
//!   "description": "This implements JWT-based...",
//!   "authors": [{"name": "Alice", "email": "alice@example.com"}],
//!   "timestamp": "2024-01-15T15:30:45Z",
//!   "dependencies": ["DEF987...", "GHI111..."],
//!   "hunks": [...],
//!   "provenance": null
//! }
//! ```
//!
//! # Change Identification
//!
//! Changes can be identified in three ways:
//!
//! 1. **Full Hash**: The complete 52-character Base32 hash
//! 2. **Hash Prefix**: An unambiguous prefix (minimum 4 characters)
//! 3. **Sequence Number**: A numeric index in the view's history (e.g., `42` or `#42`)
//!
//! If no identifier is provided, the most recent change on the current view is shown.
//!
//! # Examples
//!
//! Show details for a specific change by hash:
//! ```text
//! $ atomic change ABC12345
//! ```
//!
//! Show the most recent change:
//! ```text
//! $ atomic change
//! ```
//!
//! Show change by sequence number:
//! ```text
//! $ atomic change 42
//! $ atomic change #42
//! ```
//!
//! Show change in JSON format:
//! ```text
//! $ atomic change ABC12345 -f json
//! ```
//!
//! # Exit Codes
//!
//! - `0`: Success
//! - `1`: Error (change not found, ambiguous hash, etc.)

use clap::{Parser, ValueEnum};
use serde::Serialize;

use atomic_core::change::{Author, Change, GraphOp, Provenance};
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::{Base32, Hash};
use atomic_repository::history::{find_change_sequence, get_change_at_sequence};
use atomic_repository::Repository;

use crate::commands::{
    find_repository_root, format_hash_with_length, format_timestamp, Command, DEFAULT_HASH_LENGTH,
};
use crate::error::{CliError, CliResult};
use crate::output::{
    author as style_author, emphasis, hash as style_hash, hint, info, path as style_path,
    timestamp as style_timestamp,
};

// Output Format

mod command;
mod types;

pub use command::*;
pub use types::*;

#[cfg(test)]
mod tests;

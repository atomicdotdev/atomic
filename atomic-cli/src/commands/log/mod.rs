//! The `log` command for viewing change history.
//!
//! This module implements the `atomic log` command, which displays the
//! history of changes applied to a view. It supports multiple output
//! formats, filtering, and pagination.
//!
//! # Usage
//!
//! ```text
//! atomic log [OPTIONS]
//!
//! Options:
//!   -n, --count <N>        Show only the last N changes
//!       --all              Show all views' history
//!       --view <NAME>      Show history for specific view
//!       --tags-only        Only show tagged changes
//!   -f, --format <FORMAT>  Output format (default, short, oneline, json)
//!       --reverse          Show in reverse order (oldest first)
//!       --from <SEQ>       Start from sequence number
//!   -h, --help             Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Default Format
//!
//! The default format provides detailed information about each change:
//!
//! ```text
//! change ABC123456789
//! Author: Alice <alice@example.com>
//! Date:   Mon Jan 15 10:30:45 2024 -0500
//!
//!     Add authentication module
//!
//!     This implements JWT-based authentication for the API.
//!     Includes token generation and validation.
//!
//! change DEF987654321
//! Author: Bob <bob@example.com>
//! Date:   Sun Jan 14 15:22:30 2024 -0500
//!
//!     Fix login redirect bug
//! ```
//!
//! ## Short Format (-f short)
//!
//! Shows hash and first line of message:
//!
//! ```text
//! ABC12345 Add authentication module
//! DEF98765 Fix login redirect bug
//! GHI11111 Update dependencies
//! ```
//!
//! ## Oneline Format (-f oneline)
//!
//! Compact single-line format with hash, date, author, and message:
//!
//! ```text
//! ABC12345 2024-01-15 Alice Add authentication module
//! DEF98765 2024-01-14 Bob   Fix login redirect bug
//! GHI11111 2024-01-13 Alice Update dependencies
//! ```
//!
//! ## JSON Format (-f json)
//!
//! Machine-readable JSON output for scripting:
//!
//! ```text
//! [
//!   {
//!     "sequence": 42,
//!     "hash": "ABC123456789...",
//!     "state": "XYZ789...",
//!     "message": "Add authentication module",
//!     "authors": [{"name": "Alice", "email": "alice@example.com"}],
//!     "timestamp": "2024-01-15T15:30:45Z",
//!     "is_tagged": false
//!   }
//! ]
//! ```
//!
//! # Examples
//!
//! Show last 10 changes:
//! ```text
//! $ atomic log -n 10
//! ```
//!
//! Show changes on a specific view:
//! ```text
//! $ atomic log --view feature-auth
//! ```
//!
//! Show all tagged changes in short format:
//! ```text
//! $ atomic log --tags-only -f short
//! ```
//!
//! # Exit Codes
//!
//! - `0`: Success
//! - `1`: Error (repository not found, view not found, etc.)

use clap::{Parser, ValueEnum};
use serde::Serialize;

use atomic_core::change::Author;

use atomic_core::types::Base32;
use atomic_repository::history::{HistoryEntry, HistoryOptions};
use atomic_repository::Repository;

use crate::commands::{
    find_repository_root, format_hash_with_length, format_timestamp, Command, DEFAULT_HASH_LENGTH,
};
use crate::error::{CliError, CliResult};
use crate::output::{
    author as style_author, hash as style_hash, hint, print_hint, timestamp as style_timestamp,
    view as style_view, warning,
};

// Output Format

mod command;
mod types;

pub use command::*;
pub use types::*;

#[cfg(test)]
mod tests;

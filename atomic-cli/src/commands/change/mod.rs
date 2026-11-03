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
//! The default format groups change metadata and evidence in a terminal tree.
//! Directory names end in `/`; textual badges describe recorded operations,
//! without folder emoji or a dependence on color. `--show-hunks` expands the
//! per-path graph summaries and `--show-deps` includes dependency messages.
//!
//! ```text
//! change ABC123456789
//! ├── Metadata
//! │   ├── Author: Alice <alice@example.com>
//! │   └── Date: 2024-01-15 10:30:45
//! ├── Message
//! │   └── Add authentication module
//! ├── Graph: +12 vertices, ~3 edges, 420 bytes
//! └── Files changed: 3
//!     └── src/
//!         ├── auth/
//!         │   ├── jwt.rs [added]
//!         │   └── mod.rs [added]
//!         └── lib.rs [modified]
//! ```
//!
//! Other badges include `[deleted]`, `[moved]`, `[restored]`, `[attributes]`,
//! `[resolved]` and `[conflicted]`. Unchanged parent directories are grouping
//! nodes without a badge, not assertions that those directories were changed.
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
//!   "provenance": null,
//!   "ledger": [
//!     {
//!       "graph_hash": "...",
//!       "session_id": "...",
//!       "agent_name": "opencode",
//!       "node_count": 12,
//!       "edge_count": 11,
//!       "nodes": [{"id": "...", "kind": "goal", "summary": "..."}],
//!       "edges": [{"from": "...", "to": "...", "kind": "led_to"}]
//!     }
//!   ]
//! }
//! ```
//!
//! The `ledger` array carries the causal decision graph(s) explaining the
//! change (the machine-readable form of the `Change Ledger` tree branch).
//! It is omitted entirely when no provenance graph is recorded.
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

use atomic_core::change::{Author, Change, GraphOp, Provenance, ProvenanceGraph};
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::{Base32, Hash};
use atomic_repository::history::{find_change_sequence, get_change_at_sequence};
use atomic_repository::{extract_move_evidence, LossNote, MoveAuthority, MoveBasis, Repository};

use crate::commands::{
    find_repository_root, format_hash_with_length, format_timestamp, Command, DEFAULT_HASH_LENGTH,
};
use crate::error::{CliError, CliResult};
use crate::output::{
    author as style_author, hash as style_hash, hint, timestamp as style_timestamp, truncate_bytes,
};

// Output Format

pub(crate) mod command;
mod tree;
pub(crate) mod types;

pub use command::*;
pub use types::*;
// The per-file hunk summaries over a change's hunks live with the triage
// projection in the repository layer now; the change command renders them.
pub use atomic_repository::triage::{hunk_display_summaries, HunkDisplaySummary};

#[cfg(test)]
mod tests;

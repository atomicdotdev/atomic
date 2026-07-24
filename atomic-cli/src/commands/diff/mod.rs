//! The `diff` command for showing changes between working copy and repository.
//!
//! This module implements the `atomic diff` command, which displays the
//! differences between the working copy and the recorded repository state.
//! It supports multiple output formats and can compare specific files or
//! show all uncommitted changes.
//!
//! # Token-Level Diff (Word Diff)
//!
//! Atomic supports **token-level diff** via the `--word-diff` flag, which shows
//! exactly which tokens/words changed within a line, not just that the line changed.
//! This is powered by the CRDT tokenization engine and is especially useful for
//! code reviews.
//!
//! ```text
//! $ atomic diff --word-diff
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! @@ -1,3 +1,3 @@
//!  fn main() {
//! -    println!("Hello");
//! +    println!("Hello, World!");
//!                     ^^^^^^^^^ <- token-level highlight
//!  }
//! ```
//!
//! # Usage
//!
//! ```text
//! atomic diff [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Specific files to diff (default: all modified files)
//!
//! Options:
//!   -c, --change <HASH>       Compare against a specific change
//!       --algorithm <ALG>     Diff algorithm (myers, patience) [default: myers]
//!       --context <N>         Number of context lines [default: 3]
//!       --stat                Show diffstat summary only
//!       --no-color            Disable colored output
//!       --word-diff           Enable token-level diff highlighting
//!       --name-only           Show only names of changed files
//!       --name-status         Show names and status of changed files
//!   -h, --help                Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Default (Unified Diff)
//!
//! Shows the traditional unified diff format with colored output:
//!
//! ```text
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! @@ -1,5 +1,6 @@
//!  fn main() {
//! -    println!("Hello");
//! +    println!("Hello, World!");
//! +    println!("Welcome!");
//!  }
//! ```
//!
//! ## Stat Format (--stat)
//!
//! Shows a summary of changes with insertion/deletion counts:
//!
//! ```text
//!  src/main.rs    | 3 ++-
//!  src/lib.rs     | 5 +++++
//!  2 files changed, 7 insertions(+), 1 deletion(-)
//! ```
//!
//! ## Name Only (--name-only)
//!
//! Shows just the filenames that have changes:
//!
//! ```text
//! src/main.rs
//! src/lib.rs
//! ```
//!
//! ## Name Status (--name-status)
//!
//! Shows filenames with their change status:
//!
//! ```text
//! M  src/main.rs
//! A  src/lib.rs
//! D  src/old.rs
//! ```
//!
//! # Examples
//!
//! Show all uncommitted changes:
//! ```text
//! $ atomic diff
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! ...
//! ```
//!
//! Show changes for specific file:
//! ```text
//! $ atomic diff src/main.rs
//! diff --atomic a/src/main.rs b/src/main.rs
//! ...
//! ```
//!
//! Show only a summary:
//! ```text
//! $ atomic diff --stat
//!  src/main.rs | 3 ++-
//!  1 file changed, 2 insertions(+), 1 deletion(-)
//! ```
//!
//! Use patience algorithm for better diffs:
//! ```text
//! $ atomic diff --algorithm patience
//! ...
//! ```
//!
//! # Token-Level Diff for Code Reviews
//!
//! The `--word-diff` flag enables fine-grained highlighting that shows exactly
//! which tokens changed within a line. This is especially useful for:
//!
//! - **Variable renames**: See `oldName` → `newName` highlighted
//! - **Parameter changes**: See which arguments were added/removed
//! - **String modifications**: See exactly which part of a string changed
//! - **Operator changes**: See `==` → `===` or `+` → `-`
//!
//! ```text
//! $ atomic diff --word-diff src/auth.rs
//! diff --atomic a/src/auth.rs b/src/auth.rs
//! @@ -10,3 +10,3 @@
//! -    let token = generate_token(user_id, 3600);
//! +    let token = generate_token(user_id, 7200, true);
//!                                          ^^^^  ^^^^^ <- changed tokens
//! ```
//!
//! The highlighting uses ANSI escape codes:
//! - **Deleted tokens**: Bright red with underline
//! - **Added tokens**: Bright green with underline
//! - **Context**: Dim red/green for unchanged parts of changed lines

use std::cmp;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};

use atomic_core::change::Change;
use atomic_core::crdt::{BranchOp, LeafOp, TrunkOp};
use atomic_core::diff::display::LineStatus;
use atomic_core::diff::semantic::{semantic_diff, LineChange, TokenChange};
use atomic_core::diff::{compute_inline_diff, diff_text, Algorithm, DiffOp, DiffResult, HunkKind};
use atomic_core::types::{Base32, Hash};
use atomic_repository::status::{FileStatus, StatusOptions};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command, DEFAULT_HASH_LENGTH};
use crate::error::{CliError, CliResult};
use crate::output::{
    added, deleted, emphasis, hash, info, modified, path as style_path, print_info,
};

mod command;
mod format;
mod helpers;
mod output;
mod types;

pub use command::*;
pub(crate) use output::{build_hunks_from_diff, format_stat_graph};
pub use types::*;

#[cfg(test)]
mod tests;

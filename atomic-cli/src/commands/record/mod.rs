//! The `record` command for creating changes from working copy modifications.
//!
//! This module implements the `atomic record` command, which creates a new
//! change from modifications in the working copy. It detects added, modified,
//! and deleted files, generates diffs, and assembles them into a change that
//! can be saved and applied to the repository graph.
//!
//! # Usage
//!
//! ```text
//! atomic record [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Specific files to record (default: all modified tracked files)
//!
//! Options:
//!   -m, --message <MESSAGE>    Commit message describing the change
//!   -a, --all                  Record all changes (including untracked files)
//!       --author <AUTHOR>      Override the author for this change
//!   -e, --edit                 Open editor for commit message
//!       --algorithm <ALG>      Diff algorithm (myers, patience) [default: myers]
//!       --dry-run              Show what would be recorded without recording
//!   -h, --help                 Print help information
//! ```
//!
//! # Output
//!
//! On success, the command displays information about the recorded change,
//! including CRDT token-level statistics for fine-grained change tracking:
//!
//! ```text
//! [dev 1/abc123] Fix authentication bug
//!  2 files changed, +4 vertices, ~2 edges, 256 bytes
//!  18 lines (+15 -3 ~0)
//!  45 tokens (+42 -3 ~0)
//!  src/auth/token.rs
//!  src/auth/login.rs
//! ```
//!
//! The CRDT statistics show:
//! - **Lines**: Total line changes with breakdown (+added -deleted ~modified)
//! - **Tokens**: Token-level operations (+added -deleted ~replaced)
//!
//! Token-level tracking enables conflict-free merging and accurate blame.
//!
//! # Examples
//!
//! Record all changes with a message:
//! ```text
//! $ atomic record -m "Add new authentication module"
//! [dev 1/abc123] Add new authentication module
//!  3 files changed, +6 vertices, ~3 edges, 512 bytes
//!  25 lines (+25 -0 ~0)
//!  87 tokens (+87 -0 ~0)
//! ```
//!
//! Record specific files:
//! ```text
//! $ atomic record src/main.rs src/lib.rs -m "Refactor entry points"
//! [dev 2/def456] Refactor entry points
//!  2 files changed, +3 vertices, ~4 edges, 384 bytes
//!  12 lines (+8 -2 ~2)
//!  34 tokens (+28 -4 ~2)
//! ```
//!
//! Dry run to preview changes:
//! ```text
//! $ atomic record --dry-run
//! Would record:
//!   modified:   src/lib.rs
//!   new file:   src/utils.rs
//! ```
//!
//! # Interactive Mode
//!
//! When no message is provided and stdin is a terminal, the command opens
//! an editor (defined by `$EDITOR` or `$VISUAL`) for entering the commit
//! message.

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;

use atomic_core::change::{AITool, AIVendor, Author, ChangeHeader, Provenance, SuggestionType};
use atomic_core::diff::Algorithm;
use atomic_core::types::Base32;
use atomic_identity::{Identity, IdentityStore, IdentityUsage};
use atomic_repository::record::{RecordOptions, RecordOutcome};
use atomic_repository::status::{FileStatus, StatusOptions};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command, DEFAULT_HASH_LENGTH};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_warning};

// Record Command

/// Record changes to the repository.
///
/// Creates a new change from the current state of tracked files.
/// The change captures all modifications, additions, and deletions
/// since the last recorded state.
#[derive(Parser, Debug, Clone)]
#[command(name = "record")]
pub struct Record {
    /// Commit message describing the change.
    ///
    /// If not provided and stdin is a terminal, opens an editor.
    /// If not provided and stdin is not a terminal, uses a default message.
    #[arg(short, long)]
    pub message: Option<String>,

    /// Record all changes including untracked files.
    ///
    /// This is equivalent to running `atomic add -A` before recording.
    /// All untracked files will be added to tracking before the change
    /// is created.
    #[arg(short, long)]
    pub all: bool,

    /// Specific files to record.
    ///
    /// If provided, only changes to these files will be included.
    /// Files must be tracked (use `atomic add` first).
    #[arg()]
    pub files: Vec<String>,

    /// Override the author for this change.
    ///
    /// Format: "Name <email>" or just "Name"
    /// If not provided, uses the default identity from the identity store.
    #[arg(long)]
    pub author: Option<String>,

    /// Use a specific identity for this change.
    ///
    /// Specify the identity name to use instead of the default.
    /// This takes precedence over --author.
    #[arg(long, short = 'i')]
    pub identity: Option<String>,

    /// Use the default identity for a specific usage context.
    ///
    /// Options: personal, work, community, bot
    /// Falls back to global default if no identity is set for this usage.
    #[arg(long)]
    pub usage: Option<String>,

    /// Open editor for commit message.
    ///
    /// Opens the editor defined by $EDITOR or $VISUAL environment
    /// variables (falls back to 'vi' on Unix, 'notepad' on Windows).
    #[arg(short, long)]
    pub edit: bool,

    /// Diff algorithm to use.
    ///
    /// - myers: Standard LCS-based diff (fast, good for most cases)
    /// - patience: Better for code with moved blocks
    #[arg(long, default_value = "myers")]
    pub algorithm: String,

    /// Show what would be recorded without actually recording.
    ///
    /// Displays the files that would be included in the change
    /// without creating the change.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip binary files instead of failing.
    ///
    /// By default, binary files cause an error. With this flag,
    /// they are silently skipped.
    #[arg(long)]
    pub skip_binary: bool,

    /// Maximum file size to record (in bytes).
    ///
    /// Files larger than this limit will be skipped or cause an error
    /// depending on the --skip-binary flag.
    #[arg(long)]
    pub max_size: Option<u64>,

    // AI Provenance Flags
    /// Mark this change as AI-assisted.
    ///
    /// Enables AI provenance tracking for this change. When set, the change
    /// will include metadata about AI involvement. Use with --ai-provider
    /// and --ai-model to specify the AI system used.
    ///
    /// Can also be set via ATOMIC_AI_ENABLED=true environment variable.
    #[arg(long = "ai-assisted")]
    pub ai_assisted: bool,

    /// AI provider/vendor name.
    ///
    /// Identifies the AI service provider (e.g., anthropic, openai, google).
    /// Required when --ai-assisted is set.
    ///
    /// Can also be set via ATOMIC_AI_PROVIDER environment variable.
    #[arg(long = "ai-provider")]
    pub ai_provider: Option<String>,

    /// AI model identifier.
    ///
    /// The specific model used (e.g., claude-sonnet-4-20250514, gpt-4).
    ///
    /// Can also be set via ATOMIC_AI_MODEL environment variable.
    #[arg(long = "ai-model")]
    pub ai_model: Option<String>,

    /// AI tool type.
    ///
    /// How the AI was used: api, chat, editor, ide-plugin, cli, ci, code-review
    ///
    /// Can also be set via ATOMIC_AI_TOOL environment variable.
    #[arg(long = "ai-tool")]
    pub ai_tool: Option<String>,

    /// Type of AI suggestion/contribution.
    ///
    /// Options: complete, partial, collaborative, selection, review,
    /// documentation, debugging, refactoring, testing
    ///
    /// Can also be set via ATOMIC_AI_SUGGESTION_TYPE environment variable.
    #[arg(long = "ai-suggestion-type")]
    pub ai_suggestion_type: Option<String>,

    /// Input tokens used by the AI.
    ///
    /// Can also be set via ATOMIC_AI_INPUT_TOKENS environment variable.
    #[arg(long = "ai-input-tokens")]
    pub ai_input_tokens: Option<u64>,

    /// Output tokens generated by the AI.
    ///
    /// Can also be set via ATOMIC_AI_OUTPUT_TOKENS environment variable.
    #[arg(long = "ai-output-tokens")]
    pub ai_output_tokens: Option<u64>,

    /// Cost of AI generation in USD.
    ///
    /// Can also be set via ATOMIC_AI_COST_USD environment variable.
    #[arg(long = "ai-cost-usd")]
    pub ai_cost_usd: Option<f64>,

    /// AI request ID (for auditing).
    ///
    /// Can also be set via ATOMIC_AI_REQUEST_ID environment variable.
    #[arg(long = "ai-request-id")]
    pub ai_request_id: Option<String>,

    /// AI session/conversation ID.
    ///
    /// Can also be set via ATOMIC_AI_SESSION_ID environment variable.
    #[arg(long = "ai-session-id")]
    pub ai_session_id: Option<String>,
}

mod builder;
mod command;
mod format;
mod provenance;

pub(super) use command::*;

#[cfg(test)]
mod tests;

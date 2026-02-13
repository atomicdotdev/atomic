#![allow(dead_code)]
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
//!       --stack <NAME>     Stack to use for sequence lookup (default: current)
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
//! 3. **Sequence Number**: A numeric index in the stack's history (e.g., `42` or `#42`)
//!
//! If no identifier is provided, the most recent change on the current stack is shown.
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
use atomic_core::pristine::StackTxnT;
use atomic_core::types::{Base32, Hash};
use atomic_repository::history::{find_change_sequence, get_change_at_sequence};
use atomic_repository::Repository;

use crate::commands::{
    find_repository_root, format_hash_with_length, format_timestamp, Command, DEFAULT_HASH_LENGTH,
};
use crate::error::{CliError, CliResult};
use crate::output::{
    author as style_author, emphasis, hash as style_hash, hint, info, path as style_path, timestamp as style_timestamp,
};

// =============================================================================
// Output Format
// =============================================================================

/// Output format for the change command.
///
/// This enum defines the available output formats for displaying change details.
/// Each format is optimized for different use cases: human reading, quick scanning,
/// or machine parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ChangeFormat {
    /// Full detailed format with all information.
    ///
    /// Shows hash, author, date, message, description, dependencies, and hunks summary.
    #[default]
    Default,

    /// Short single-line format.
    ///
    /// Shows hash, date, author, and message first line.
    Short,

    /// JSON format for machine parsing.
    ///
    /// Outputs a JSON object with all change metadata.
    Json,
}

impl std::fmt::Display for ChangeFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeFormat::Default => write!(f, "default"),
            ChangeFormat::Short => write!(f, "short"),
            ChangeFormat::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for ChangeFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" | "full" => Ok(ChangeFormat::Default),
            "short" => Ok(ChangeFormat::Short),
            "json" => Ok(ChangeFormat::Json),
            _ => Err(format!(
                "Invalid format '{}'. Expected: default, short, json",
                s
            )),
        }
    }
}

// =============================================================================
// Change Identifier
// =============================================================================

/// Parsed change identifier.
///
/// A change can be identified by its hash (full or prefix) or by its
/// sequence number in a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeIdentifier {
    /// Full 52-character Base32 hash.
    FullHash(Hash),

    /// Hash prefix (4-51 characters).
    HashPrefix(String),

    /// Sequence number in the stack's history.
    Sequence(u64),

    /// No identifier - use most recent change.
    Latest,
}

impl ChangeIdentifier {
    /// Parse a change identifier from a string.
    ///
    /// # Arguments
    ///
    /// * `s` - The identifier string
    ///
    /// # Returns
    ///
    /// The parsed `ChangeIdentifier`.
    ///
    /// # Parsing Rules
    ///
    /// - If the string is empty or None, returns `Latest`
    /// - If the string starts with `#`, parses as sequence number
    /// - If the string is purely numeric, parses as sequence number
    /// - If the string is exactly 52 base32 characters, parses as `FullHash`
    /// - Otherwise, parses as `HashPrefix`
    pub fn parse(s: Option<&str>) -> Result<Self, String> {
        let s = match s {
            None => return Ok(ChangeIdentifier::Latest),
            Some(s) if s.is_empty() => return Ok(ChangeIdentifier::Latest),
            Some(s) => s.trim(),
        };

        // Check for sequence number with # prefix
        if let Some(num_str) = s.strip_prefix('#') {
            return num_str
                .parse::<u64>()
                .map(ChangeIdentifier::Sequence)
                .map_err(|_| format!("Invalid sequence number: {}", num_str));
        }

        // Check for pure numeric (sequence number)
        if s.chars().all(|c| c.is_ascii_digit()) {
            return s
                .parse::<u64>()
                .map(ChangeIdentifier::Sequence)
                .map_err(|_| format!("Invalid sequence number: {}", s));
        }

        // Check for valid Base32 characters
        let upper = s.to_uppercase();
        if !upper
            .chars()
            .all(|c| "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".contains(c))
        {
            return Err(format!(
                "Invalid hash characters in '{}'. Hashes use Base32 encoding (A-Z, 2-7).",
                s
            ));
        }

        // Full hash (52 characters)
        if upper.len() == 52 {
            return Hash::from_base32(upper.as_bytes())
                .map(ChangeIdentifier::FullHash)
                .ok_or_else(|| format!("Invalid Base32 hash: {}", s));
        }

        // Hash prefix (4-51 characters)
        if upper.len() >= 4 {
            Ok(ChangeIdentifier::HashPrefix(upper))
        } else {
            Err(format!(
                "Hash prefix too short: '{}'. Minimum 4 characters required.",
                s
            ))
        }
    }

    /// Check if this identifier is for the latest change.
    pub fn is_latest(&self) -> bool {
        matches!(self, ChangeIdentifier::Latest)
    }

    /// Check if this identifier is a sequence number.
    pub fn is_sequence(&self) -> bool {
        matches!(self, ChangeIdentifier::Sequence(_))
    }

    /// Check if this identifier is a hash (full or prefix).
    pub fn is_hash(&self) -> bool {
        matches!(
            self,
            ChangeIdentifier::FullHash(_) | ChangeIdentifier::HashPrefix(_)
        )
    }
}

// =============================================================================
// JSON Output Types
// =============================================================================

/// JSON representation of an author.
#[derive(Debug, Clone, Serialize)]
pub struct JsonAuthor {
    /// The author's name.
    pub name: String,
    /// The author's email (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl From<&Author> for JsonAuthor {
    fn from(author: &Author) -> Self {
        Self {
            name: author.name.clone(),
            email: author.email.clone(),
        }
    }
}

/// JSON representation of a graph_op summary.
#[derive(Debug, Clone, Serialize)]
pub struct JsonHunkSummary {
    /// Type of graph_op (FileAdd, FileDel, FileMove, Edit, etc.)
    pub hunk_type: String,
    /// Path affected (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// JSON representation of a change.
#[derive(Debug, Clone, Serialize)]
pub struct JsonChange {
    /// The full Base32 hash.
    pub hash: String,

    /// The change message.
    pub message: String,

    /// The description (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The authors.
    pub authors: Vec<JsonAuthor>,

    /// The timestamp (ISO 8601 format).
    pub timestamp: String,

    /// Dependencies (as Base32 hashes).
    pub dependencies: Vec<String>,

    /// GraphOp summaries.
    pub hunks: Vec<JsonHunkSummary>,

    /// Whether this change has AI provenance information.
    pub has_provenance: bool,

    /// AI provenance details (if available and requested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<JsonProvenance>,

    /// Sequence number in the current stack (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

impl JsonChange {
    /// Create a JSON representation from a change and its hash.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to represent
    /// * `hash` - The change's hash
    /// * `sequence` - Optional sequence number in the stack
    pub fn from_change(change: &Change, hash: &Hash, sequence: Option<u64>) -> Self {
        Self {
            hash: hash.to_base32(),
            message: change.hashed.header.message.clone(),
            description: change.hashed.header.description.clone(),
            authors: change
                .hashed
                .header
                .authors
                .iter()
                .map(JsonAuthor::from)
                .collect(),
            timestamp: change.hashed.header.timestamp.to_rfc3339(),
            dependencies: change
                .hashed
                .dependencies
                .iter()
                .map(|h| h.to_base32())
                .collect(),
            hunks: change
                .hashed
                .hunks
                .iter()
                .map(|h| hunk_to_summary(h))
                .collect(),
            has_provenance: change.has_provenance(),
            provenance: None,
            sequence,
        }
    }

    /// Create from a Change with provenance details.
    pub fn from_change_with_provenance(
        change: &Change,
        hash: &Hash,
        sequence: Option<u64>,
    ) -> Self {
        let mut json = Self::from_change(change, hash, sequence);
        if let Some(prov) = change.hashed.provenance.first() {
            json.provenance = Some(JsonProvenance::from(prov));
        }
        json
    }
}

/// JSON representation of AI provenance.
#[derive(Debug, Clone, Serialize)]
pub struct JsonProvenance {
    /// AI vendor/provider.
    pub vendor: String,
    /// Model identifier.
    pub model: String,
    /// Model version (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    /// Tool used to interact with the AI.
    pub tool: String,
    /// Type of AI contribution.
    pub suggestion_type: String,
    /// Token usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<JsonTokenUsage>,
    /// Cost information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<JsonCost>,
    /// Temperature setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Request timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Request ID from provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// JSON representation of token usage.
#[derive(Debug, Clone, Serialize)]
pub struct JsonTokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// JSON representation of cost.
#[derive(Debug, Clone, Serialize)]
pub struct JsonCost {
    pub amount_micros: u64,
    pub currency: String,
}

impl From<&Provenance> for JsonProvenance {
    fn from(prov: &Provenance) -> Self {
        let tokens = if !prov.tokens.is_empty() {
            Some(JsonTokenUsage {
                input: Some(prov.tokens.input_tokens),
                output: Some(prov.tokens.output_tokens),
                total: Some(prov.tokens.total_tokens),
            })
        } else {
            None
        };

        let cost = if !prov.cost.is_zero() {
            Some(JsonCost {
                amount_micros: prov.cost.micro_usd,
                currency: "USD".to_string(),
            })
        } else {
            None
        };

        Self {
            vendor: format!("{:?}", prov.vendor),
            model: prov.model.clone(),
            model_version: prov.model_version.clone(),
            tool: format!("{:?}", prov.tool),
            suggestion_type: format!("{:?}", prov.suggestion_type),
            tokens,
            cost,
            temperature: prov.temperature.map(|t| t as f64 / 1000.0),
            timestamp: prov.timestamp,
            request_id: prov.request_id.clone(),
            session_id: prov.session_id.clone(),
        }
    }
}

/// Count the total atoms (vertices and edges) in hunks.
fn count_atoms<H>(hunks: &[GraphOp<H>]) -> (usize, usize) {
    let mut vertices = 0;
    let mut edges = 0;

    for graph_op in hunks {
        match graph_op {
            GraphOp::FileAdd { contents, .. } => {
                // FileAdd creates: name span, inode span, and optionally content span
                vertices += 2;
                if contents.is_some() {
                    vertices += 1;
                }
            }
            GraphOp::FileDel { contents, .. } => {
                // FileDel creates edge maps to mark as deleted
                edges += 1;
                if contents.is_some() {
                    edges += 1;
                }
            }
            GraphOp::FileUndel { contents, .. } => {
                // FileUndel creates edge maps to resurrect
                edges += 1;
                if contents.is_some() {
                    edges += 1;
                }
            }
            GraphOp::FileMove { .. } => {
                // FileMove: delete old name edge, add new name span
                vertices += 1;
                edges += 1;
            }
            GraphOp::DirAdd { .. } => {
                // DirAdd creates: name span, inode span (no content)
                vertices += 2;
            }
            GraphOp::DirDel { .. } => {
                // DirDel creates edge maps to mark as deleted
                edges += 1;
            }
            GraphOp::DirUndel { .. } => {
                // DirUndel creates edge maps to resurrect
                edges += 1;
            }
            GraphOp::Edit { .. } => {
                // Edit creates a new content span
                vertices += 1;
            }
            GraphOp::Replacement { .. } => {
                // Replacement: new span + edge map for deletion
                vertices += 1;
                edges += 1;
            }
            GraphOp::SolveNameConflict { .. } | GraphOp::UnsolveNameConflict { .. } => {
                edges += 1;
            }
            GraphOp::SolveOrderConflict { .. } | GraphOp::UnsolveOrderConflict { .. } => {
                edges += 1;
            }
            GraphOp::ResurrectZombies { .. } => {
                edges += 1;
            }
            GraphOp::AddRoot { .. } => {
                vertices += 1;
            }
            GraphOp::DelRoot { .. } => {
                edges += 1;
            }
        }
    }

    (vertices, edges)
}

/// Get a short description of the atoms in a graph_op.
fn hunk_atom_info<H>(graph_op: &GraphOp<H>) -> String {
    match graph_op {
        GraphOp::FileAdd { contents, .. } => {
            if contents.is_some() {
                "(+3 vertices: name, inode, content)".to_string()
            } else {
                "(+2 vertices: name, inode)".to_string()
            }
        }
        GraphOp::FileDel { .. } => "(~edges: mark deleted)".to_string(),
        GraphOp::FileUndel { .. } => "(~edges: resurrect)".to_string(),
        GraphOp::FileMove { .. } => "(+1 span, ~1 edge: rename)".to_string(),
        GraphOp::DirAdd { .. } => "(+2 vertices: name, inode)".to_string(),
        GraphOp::DirDel { .. } => "(~edges: mark deleted)".to_string(),
        GraphOp::DirUndel { .. } => "(~edges: resurrect)".to_string(),
        GraphOp::Edit { .. } => "(+1 span: new content)".to_string(),
        GraphOp::Replacement { .. } => "(+1 span, ~1 edge: replace)".to_string(),
        GraphOp::SolveNameConflict { .. } => "(~edges: resolve name conflict)".to_string(),
        GraphOp::UnsolveNameConflict { .. } => "(~edges: unresolve name conflict)".to_string(),
        GraphOp::SolveOrderConflict { .. } => "(~edges: resolve order conflict)".to_string(),
        GraphOp::UnsolveOrderConflict { .. } => "(~edges: unresolve order conflict)".to_string(),
        GraphOp::ResurrectZombies { .. } => "(~edges: resurrect zombies)".to_string(),
        GraphOp::AddRoot { .. } => "(+1 span: root)".to_string(),
        GraphOp::DelRoot { .. } => "(~edges: delete root)".to_string(),
    }
}

/// Convert a graph_op to a summary for JSON output.
fn hunk_to_summary<H>(graph_op: &GraphOp<H>) -> JsonHunkSummary {
    match graph_op {
        GraphOp::FileAdd { path, .. } => JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileDel { path, .. } => JsonHunkSummary {
            hunk_type: "FileDel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileMove { path, .. } => JsonHunkSummary {
            hunk_type: "FileMove".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::FileUndel { path, .. } => JsonHunkSummary {
            hunk_type: "FileUndel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirAdd { path, .. } => JsonHunkSummary {
            hunk_type: "DirAdd".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirDel { path, .. } => JsonHunkSummary {
            hunk_type: "DirDel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::DirUndel { path, .. } => JsonHunkSummary {
            hunk_type: "DirUndel".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::Edit { local, .. } => JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::Replacement { local, .. } => JsonHunkSummary {
            hunk_type: "Replacement".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::SolveNameConflict { path, .. } => JsonHunkSummary {
            hunk_type: "SolveNameConflict".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::UnsolveNameConflict { path, .. } => JsonHunkSummary {
            hunk_type: "UnsolveNameConflict".to_string(),
            path: Some(path.clone()),
        },
        GraphOp::SolveOrderConflict { local, .. } => JsonHunkSummary {
            hunk_type: "SolveOrderConflict".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::UnsolveOrderConflict { local, .. } => JsonHunkSummary {
            hunk_type: "UnsolveOrderConflict".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::ResurrectZombies { local, .. } => JsonHunkSummary {
            hunk_type: "ResurrectZombies".to_string(),
            path: Some(local.path.clone()),
        },
        GraphOp::AddRoot { .. } => JsonHunkSummary {
            hunk_type: "AddRoot".to_string(),
            path: None,
        },
        GraphOp::DelRoot { .. } => JsonHunkSummary {
            hunk_type: "DelRoot".to_string(),
            path: None,
        },
    }
}

// =============================================================================
// Change Command
// =============================================================================

/// Show details for a specific change.
///
/// The `change` command displays detailed information about a change (patch)
/// in the repository. Changes can be identified by:
///
/// - **Full hash**: The complete 52-character Base32 hash
/// - **Hash prefix**: An unambiguous prefix (minimum 4 characters)
/// - **Sequence number**: Index in the stack's history (`42` or `#42`)
///
/// If no identifier is provided, shows the most recent change.
///
/// # Examples
///
/// ```text
/// # Show change by hash prefix
/// atomic change ABC12345
///
/// # Show most recent change
/// atomic change
///
/// # Show change by sequence number
/// atomic change 42
///
/// # Show in JSON format
/// atomic change ABC12345 -f json
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "change")]
pub struct ChangeCmd {
    /// Change identifier (hash, hash prefix, or sequence number).
    ///
    /// If omitted, shows the most recent change on the current stack.
    /// Sequence numbers can be prefixed with `#` (e.g., `#42`).
    #[arg(value_name = "HASH_OR_SEQ")]
    pub identifier: Option<String>,

    /// Stack to use for sequence lookup.
    ///
    /// When looking up by sequence number, use this stack instead
    /// of the current stack.
    #[arg(long = "stack", value_name = "NAME")]
    pub stack: Option<String>,

    /// Output format.
    ///
    /// Controls how change details are displayed:
    /// - default: Full details with formatting
    /// - short: Compact single-line format
    /// - json: Machine-readable JSON
    #[arg(short = 'f', long = "format", value_enum, default_value = "default")]
    pub format: ChangeFormat,

    /// Show dependency details.
    ///
    /// When enabled, shows the message of each dependency change.
    #[arg(long = "show-deps")]
    pub show_deps: bool,

    /// Show graph_op details.
    ///
    /// When enabled, shows detailed information about each graph_op.
    #[arg(long = "show-hunks")]
    pub show_hunks: bool,

    /// Show full hashes instead of abbreviated.
    #[arg(long = "full-hash")]
    pub full_hash: bool,

    /// Show AI provenance details.
    ///
    /// When enabled, displays detailed information about AI assistance
    /// used in creating this change, including vendor, model, token usage,
    /// and cost information.
    #[arg(short = 'p', long = "provenance")]
    pub show_provenance: bool,
}

impl ChangeCmd {
    /// Create a new ChangeCmd with default settings.
    pub fn new() -> Self {
        Self {
            identifier: None,
            stack: None,
            format: ChangeFormat::Default,
            show_deps: false,
            show_hunks: false,
            full_hash: false,
            show_provenance: false,
        }
    }

    /// Set the change identifier.
    pub fn with_identifier(mut self, id: impl Into<String>) -> Self {
        self.identifier = Some(id.into());
        self
    }

    /// Set the stack for sequence lookup.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Set the output format.
    pub fn with_format(mut self, format: ChangeFormat) -> Self {
        self.format = format;
        self
    }

    /// Enable showing dependency details.
    pub fn with_show_deps(mut self, show: bool) -> Self {
        self.show_deps = show;
        self
    }

    /// Enable showing graph_op details.
    pub fn with_show_hunks(mut self, show: bool) -> Self {
        self.show_hunks = show;
        self
    }

    /// Enable full hash display.
    pub fn with_full_hash(mut self, full: bool) -> Self {
        self.full_hash = full;
        self
    }

    /// Enable showing AI provenance details.
    pub fn with_show_provenance(mut self, show: bool) -> Self {
        self.show_provenance = show;
        self
    }

    /// Get the hash display length.
    fn get_hash_length(&self) -> usize {
        if self.full_hash {
            52
        } else {
            DEFAULT_HASH_LENGTH
        }
    }

    /// Resolve the change identifier to a hash.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository
    /// * `id` - The parsed change identifier
    ///
    /// # Returns
    ///
    /// A tuple of (hash, optional sequence number).
    fn resolve_identifier(&self, repo: &Repository) -> CliResult<(Hash, Option<u64>)> {
        let id = ChangeIdentifier::parse(self.identifier.as_deref())
            .map_err(|e| CliError::InvalidArgument { message: e })?;

        let stack_name = self
            .stack
            .as_deref()
            .unwrap_or_else(|| repo.current_stack());

        match id {
            ChangeIdentifier::FullHash(hash) => {
                // Verify the change exists
                if !repo.has_change(&hash) {
                    return Err(CliError::ChangeNotFound {
                        hash: hash.to_base32(),
                    });
                }

                // Try to find sequence number
                let seq = self.find_sequence_for_hash(repo, stack_name, &hash)?;
                Ok((hash, seq))
            }

            ChangeIdentifier::HashPrefix(prefix) => {
                let (hash, seq) = self.resolve_hash_prefix(repo, stack_name, &prefix)?;
                Ok((hash, seq))
            }

            ChangeIdentifier::Sequence(seq) => {
                let hash = self.resolve_sequence(repo, stack_name, seq)?;
                Ok((hash, Some(seq)))
            }

            ChangeIdentifier::Latest => {
                let (hash, seq) = self.get_latest_change(repo, stack_name)?;
                Ok((hash, Some(seq)))
            }
        }
    }

    /// Find the sequence number for a hash in the current stack.
    fn find_sequence_for_hash(
        &self,
        repo: &Repository,
        stack_name: &str,
        hash: &Hash,
    ) -> CliResult<Option<u64>> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        find_change_sequence(&txn, &stack, hash)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))
    }

    /// Resolve a hash prefix to a full hash.
    fn resolve_hash_prefix(
        &self,
        repo: &Repository,
        stack_name: &str,
        prefix: &str,
    ) -> CliResult<(Hash, Option<u64>)> {
        // Search for matching changes
        let mut matches: Vec<Hash> = Vec::new();

        for result in repo.iter_changes() {
            let hash = result.map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;
            let hash_str = hash.to_base32();
            if hash_str.starts_with(prefix) {
                matches.push(hash);
            }
        }

        match matches.len() {
            0 => Err(CliError::ChangeNotFound {
                hash: prefix.to_string(),
            }),
            1 => {
                let hash = matches[0];
                let seq = self.find_sequence_for_hash(repo, stack_name, &hash)?;
                Ok((hash, seq))
            }
            _ => {
                // Format the matches for display in the error message
                let match_list: Vec<String> = matches.iter().map(|h| h.to_base32()).collect();
                Err(CliError::AmbiguousHash {
                    hash: format!("{} (matches: {})", prefix, match_list.join(", ")),
                })
            }
        }
    }

    /// Resolve a sequence number to a hash.
    fn resolve_sequence(&self, repo: &Repository, stack_name: &str, seq: u64) -> CliResult<Hash> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let entry = get_change_at_sequence(&txn, &stack, seq).map_err(|e| match e {
            atomic_repository::history::HistoryError::SequenceOutOfRange { sequence, max } => {
                CliError::InvalidArgument {
                    message: format!(
                        "Sequence {} out of range. Stack has {} changes (0-{}).",
                        sequence,
                        max + 1,
                        max
                    ),
                }
            }
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
        })?;

        Ok(entry.hash)
    }

    /// Get the most recent change on the stack.
    fn get_latest_change(&self, repo: &Repository, stack_name: &str) -> CliResult<(Hash, u64)> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        if stack.change_count == 0 {
            return Err(CliError::NothingToRecord);
        }

        let seq = stack.change_count - 1;
        let entry = get_change_at_sequence(&txn, &stack, seq)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        Ok((entry.hash, seq))
    }

    /// Format the change for default output.
    fn format_default(
        &self,
        change: &Change,
        hash: &Hash,
        sequence: Option<u64>,
        repo: &Repository,
    ) -> String {
        let mut output = String::new();
        let hash_len = self.get_hash_length();

        // Change header
        let hash_str = format_hash_with_length(hash, hash_len);
        output.push_str(&format!(
            "{} {}",
            style_hash("change"),
            style_hash(&hash_str)
        ));
        if let Some(seq) = sequence {
            output.push_str(&format!(" {}", hint(&format!("(#{})", seq))));
        }
        output.push('\n');

        // Authors
        for author in &change.hashed.header.authors {
            let author_line = format_author(author);
            output.push_str(&format!("Author: {}\n", style_author(&author_line)));
        }

        // Timestamp
        let time_str = format_timestamp(&change.hashed.header.timestamp);
        output.push_str(&format!("Date:   {}\n", style_timestamp(&time_str)));

        // Message
        output.push('\n');
        for line in change.hashed.header.message.lines() {
            output.push_str(&format!("    {}\n", line));
        }

        // Description
        if let Some(ref desc) = change.hashed.header.description {
            output.push('\n');
            for line in desc.lines() {
                output.push_str(&format!("    {}\n", line));
            }
        }

        // Dependencies
        if !change.hashed.dependencies.is_empty() {
            output.push('\n');
            output.push_str(&format!(
                "Dependencies: {}\n",
                change.hashed.dependencies.len()
            ));

            for dep_hash in &change.hashed.dependencies {
                let dep_hash_str = format_hash_with_length(dep_hash, 12);
                let dep_msg = if self.show_deps {
                    repo.load_change(dep_hash)
                        .ok()
                        .map(|c| {
                            c.hashed
                                .header
                                .message
                                .lines()
                                .next()
                                .unwrap_or("")
                                .to_string()
                        })
                        .unwrap_or_else(|| "[unable to load]".to_string())
                } else {
                    String::new()
                };

                if self.show_deps {
                    output.push_str(&format!("  {}... - {}\n", dep_hash_str, dep_msg));
                } else {
                    output.push_str(&format!("  {}...\n", dep_hash_str));
                }
            }
        }

        // Graph statistics
        output.push('\n');
        let (vertices, edges) = count_atoms(&change.hashed.hunks);
        let content_bytes = change.contents.len();
        output.push_str(&format!(
            "Graph: +{} vertices, ~{} edges, {} bytes\n",
            vertices, edges, content_bytes
        ));

        // Hunks summary
        if !change.hashed.hunks.is_empty() {
            output.push_str(&format!(
                "Files changed: {}\n",
                count_unique_paths(&change.hashed.hunks)
            ));

            // Always show hunks with atom details
            for graph_op in &change.hashed.hunks {
                let (symbol, path) = hunk_symbol_and_path(graph_op);
                let atom_info = hunk_atom_info(graph_op);
                output.push_str(&format!(
                    "  {} {} {}\n",
                    symbol,
                    style_path(&path),
                    hint(&atom_info)
                ));
            }
        }

        // Provenance
        if change.has_provenance() {
            output.push('\n');
            if self.show_provenance {
                if let Some(prov) = change.hashed.provenance.first() {
                    output.push_str(&self.format_provenance(prov));
                }
            } else {
                output.push_str(&format!(
                    "{}\n",
                    hint("This change has AI provenance information (use -p to view)")
                ));
            }
        }

        output
    }

    /// Format the change for short output.
    fn format_short(&self, change: &Change, hash: &Hash, sequence: Option<u64>) -> String {
        let hash_len = self.get_hash_length();
        let hash_str = format_hash_with_length(hash, hash_len);

        let date_str = change
            .hashed
            .header
            .timestamp
            .format("%Y-%m-%d")
            .to_string();

        let author_name = change
            .hashed
            .header
            .authors
            .first()
            .map(|a| truncate_string(&a.name, 15))
            .unwrap_or_else(|| "(unknown)".to_string());

        let message = change
            .hashed
            .header
            .message
            .lines()
            .next()
            .unwrap_or("(no message)");

        // Provenance indicator for short format
        let _prov_indicator = if change.has_provenance() { " 🤖" } else { "" };

        let seq_str = sequence.map(|s| format!(" #{}", s)).unwrap_or_default();

        format!(
            "{}{} {} {:15} {}\n",
            style_hash(&hash_str),
            hint(&seq_str),
            style_timestamp(&date_str),
            style_author(&author_name),
            message
        )
    }

    /// Format AI provenance information for display.
    fn format_provenance(&self, prov: &Provenance) -> String {
        let mut output = String::new();

        output.push_str(&format!("{}\n", emphasis("AI Provenance:")));
        output.push_str(&format!(
            "  Vendor:  {}\n",
            info(&format!("{:?}", prov.vendor))
        ));
        output.push_str(&format!("  Model:   {}\n", info(&prov.model)));

        if let Some(version) = &prov.model_version {
            output.push_str(&format!("  Version: {}\n", hint(version)));
        }

        output.push_str(&format!(
            "  Tool:    {}\n",
            info(&format!("{:?}", prov.tool))
        ));
        output.push_str(&format!(
            "  Type:    {}\n",
            hint(&format!("{:?}", prov.suggestion_type))
        ));

        // Token usage
        if !prov.tokens.is_empty() {
            output.push_str("  Tokens:\n");
            output.push_str(&format!("    Input:  {}\n", prov.tokens.input_tokens));
            output.push_str(&format!("    Output: {}\n", prov.tokens.output_tokens));
            output.push_str(&format!("    Total:  {}\n", prov.tokens.total_tokens));
        }

        // Cost
        if !prov.cost.is_zero() {
            output.push_str(&format!("  Cost:    ${:.6} USD\n", prov.cost.usd));
        }

        // Temperature
        if let Some(temp) = prov.temperature {
            let temp_f = temp as f64 / 1000.0;
            output.push_str(&format!("  Temp:    {:.2}\n", temp_f));
        }

        // Request/Session IDs
        if let Some(req_id) = &prov.request_id {
            output.push_str(&format!("  Request: {}\n", hint(req_id)));
        }
        if let Some(sess_id) = &prov.session_id {
            output.push_str(&format!("  Session: {}\n", hint(sess_id)));
        }

        // Additional metadata (key-value pairs from agent recording)
        if !prov.metadata.is_empty() {
            output.push_str("  Metadata:\n");
            for (key, value) in &prov.metadata {
                output.push_str(&format!("    {}: {}\n", hint(key), info(value)));
            }
        }

        output
    }

    /// Format the change for JSON output.
    fn format_json(&self, change: &Change, hash: &Hash, sequence: Option<u64>) -> String {
        let json_change = if self.show_provenance {
            JsonChange::from_change_with_provenance(change, hash, sequence)
        } else {
            JsonChange::from_change(change, hash, sequence)
        };
        serde_json::to_string_pretty(&json_change)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e))
    }

    /// Print the change in the configured format.
    fn print_change(&self, change: &Change, hash: &Hash, sequence: Option<u64>, repo: &Repository) {
        let output = match self.format {
            ChangeFormat::Default => self.format_default(change, hash, sequence, repo),
            ChangeFormat::Short => self.format_short(change, hash, sequence),
            ChangeFormat::Json => self.format_json(change, hash, sequence),
        };
        print!("{}", output);
    }
}

impl Default for ChangeCmd {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for ChangeCmd {
    /// Execute the change command.
    ///
    /// This method:
    /// 1. Finds the repository root
    /// 2. Resolves the change identifier to a hash
    /// 3. Loads the change from the store
    /// 4. Formats and displays the change
    ///
    /// # Errors
    ///
    /// Returns a `CliError` if:
    /// - No repository is found
    /// - The change identifier is invalid
    /// - The change is not found
    /// - The hash prefix is ambiguous
    fn run(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open_readonly(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
        })?;

        // Resolve identifier
        let (hash, sequence) = self.resolve_identifier(&repo)?;

        // Load the change
        let change = repo.load_change(&hash).map_err(|e| match e {
            atomic_repository::RepositoryError::ChangeNotFound { hash } => {
                CliError::ChangeNotFound { hash }
            }
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
        })?;

        // Print the change
        self.print_change(&change, &hash, sequence, &repo);

        Ok(())
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Truncate a string to a maximum length, adding ellipsis if needed.
fn truncate_string(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

/// Format an author for display.
fn format_author(author: &Author) -> String {
    if let Some(ref email) = author.email {
        format!("{} <{}>", author.name, email)
    } else {
        author.name.clone()
    }
}

/// Count unique paths affected by hunks.
fn count_unique_paths<H>(hunks: &[GraphOp<H>]) -> usize {
    let mut paths = std::collections::HashSet::new();
    for graph_op in hunks {
        if let Some(path) = get_hunk_path(graph_op) {
            paths.insert(path);
        }
    }
    paths.len()
}

/// Get the path from a graph_op.
fn get_hunk_path<H>(graph_op: &GraphOp<H>) -> Option<String> {
    match graph_op {
        GraphOp::FileAdd { path, .. } => Some(path.clone()),
        GraphOp::FileDel { path, .. } => Some(path.clone()),
        GraphOp::FileMove { path, .. } => Some(path.clone()),
        GraphOp::FileUndel { path, .. } => Some(path.clone()),
        GraphOp::DirAdd { path, .. } => Some(path.clone()),
        GraphOp::DirDel { path, .. } => Some(path.clone()),
        GraphOp::DirUndel { path, .. } => Some(path.clone()),
        GraphOp::Edit { local, .. } => Some(local.path.clone()),
        GraphOp::Replacement { local, .. } => Some(local.path.clone()),
        GraphOp::SolveNameConflict { path, .. } => Some(path.clone()),
        GraphOp::UnsolveNameConflict { path, .. } => Some(path.clone()),
        GraphOp::SolveOrderConflict { local, .. } => Some(local.path.clone()),
        GraphOp::UnsolveOrderConflict { local, .. } => Some(local.path.clone()),
        GraphOp::ResurrectZombies { local, .. } => Some(local.path.clone()),
        GraphOp::AddRoot { .. } => None,
        GraphOp::DelRoot { .. } => None,
    }
}

/// Get symbol and path for a graph_op (for display).
fn hunk_symbol_and_path<H>(graph_op: &GraphOp<H>) -> (&'static str, String) {
    match graph_op {
        GraphOp::FileAdd { path, .. } => ("+", path.clone()),
        GraphOp::FileDel { path, .. } => ("-", path.clone()),
        GraphOp::FileMove { path, .. } => ("→", path.clone()),
        GraphOp::FileUndel { path, .. } => ("↑", path.clone()),
        GraphOp::DirAdd { path, .. } => ("📁+", path.clone()),
        GraphOp::DirDel { path, .. } => ("📁-", path.clone()),
        GraphOp::DirUndel { path, .. } => ("📁↑", path.clone()),
        GraphOp::Edit { local, .. } => ("~", local.path.clone()),
        GraphOp::Replacement { local, .. } => ("±", local.path.clone()),
        GraphOp::SolveNameConflict { path, .. } => ("✓", path.clone()),
        GraphOp::UnsolveNameConflict { path, .. } => ("!", path.clone()),
        GraphOp::SolveOrderConflict { local, .. } => ("✓", local.path.clone()),
        GraphOp::UnsolveOrderConflict { local, .. } => ("!", local.path.clone()),
        GraphOp::ResurrectZombies { local, .. } => ("↑", local.path.clone()),
        GraphOp::AddRoot { .. } => ("◉", "(root)".to_string()),
        GraphOp::DelRoot { .. } => ("⊘", "(root)".to_string()),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_core::types::Merkle;

    // =========================================================================
    // ChangeFormat Tests
    // =========================================================================

    #[test]
    fn test_change_format_default_is_default() {
        let format = ChangeFormat::default();
        assert_eq!(format, ChangeFormat::Default);
    }

    #[test]
    fn test_change_format_display() {
        assert_eq!(ChangeFormat::Default.to_string(), "default");
        assert_eq!(ChangeFormat::Short.to_string(), "short");
        assert_eq!(ChangeFormat::Json.to_string(), "json");
    }

    #[test]
    fn test_change_format_from_str_default() {
        assert_eq!(
            "default".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
        assert_eq!(
            "full".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
    }

    #[test]
    fn test_change_format_from_str_short() {
        assert_eq!(
            "short".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Short
        );
    }

    #[test]
    fn test_change_format_from_str_json() {
        assert_eq!("json".parse::<ChangeFormat>().unwrap(), ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_from_str_case_insensitive() {
        assert_eq!(
            "DEFAULT".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Default
        );
        assert_eq!(
            "SHORT".parse::<ChangeFormat>().unwrap(),
            ChangeFormat::Short
        );
        assert_eq!("JSON".parse::<ChangeFormat>().unwrap(), ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_from_str_invalid() {
        let result = "invalid".parse::<ChangeFormat>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid format"));
    }

    #[test]
    fn test_change_format_equality() {
        assert_eq!(ChangeFormat::Default, ChangeFormat::Default);
        assert_ne!(ChangeFormat::Default, ChangeFormat::Short);
        assert_ne!(ChangeFormat::Short, ChangeFormat::Json);
    }

    #[test]
    fn test_change_format_clone() {
        let format = ChangeFormat::Short;
        let cloned = format.clone();
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_change_format_copy() {
        let format = ChangeFormat::Json;
        let copied: ChangeFormat = format;
        assert_eq!(format, copied);
    }

    // =========================================================================
    // ChangeIdentifier Tests
    // =========================================================================

    #[test]
    fn test_identifier_parse_none() {
        let id = ChangeIdentifier::parse(None).unwrap();
        assert_eq!(id, ChangeIdentifier::Latest);
    }

    #[test]
    fn test_identifier_parse_empty() {
        let id = ChangeIdentifier::parse(Some("")).unwrap();
        assert_eq!(id, ChangeIdentifier::Latest);
    }

    #[test]
    fn test_identifier_parse_sequence_with_hash() {
        let id = ChangeIdentifier::parse(Some("#42")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(42));
    }

    #[test]
    fn test_identifier_parse_sequence_numeric() {
        let id = ChangeIdentifier::parse(Some("123")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(123));
    }

    #[test]
    fn test_identifier_parse_sequence_zero() {
        let id = ChangeIdentifier::parse(Some("0")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(0));
    }

    #[test]
    fn test_identifier_parse_hash_prefix() {
        let id = ChangeIdentifier::parse(Some("ABCD")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCD".to_string()));
    }

    #[test]
    fn test_identifier_parse_hash_prefix_lowercase() {
        let id = ChangeIdentifier::parse(Some("abcdefgh")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEFGH".to_string()));
    }

    #[test]
    fn test_identifier_parse_hash_prefix_mixed_case() {
        let id = ChangeIdentifier::parse(Some("AbCdEf")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEF".to_string()));
    }

    #[test]
    fn test_identifier_parse_full_hash() {
        // 52-character base32 hash
        let full_hash = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let id = ChangeIdentifier::parse(Some(full_hash)).unwrap();
        assert!(matches!(id, ChangeIdentifier::FullHash(_)));
    }

    #[test]
    fn test_identifier_parse_prefix_too_short() {
        let result = ChangeIdentifier::parse(Some("ABC"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too short"));
    }

    #[test]
    fn test_identifier_parse_invalid_characters() {
        let result = ChangeIdentifier::parse(Some("ABCD1890")); // 8, 9, 0 are invalid in base32
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid hash characters"));
    }

    #[test]
    fn test_identifier_parse_whitespace_trimmed() {
        let id = ChangeIdentifier::parse(Some("  ABCDEF  ")).unwrap();
        assert_eq!(id, ChangeIdentifier::HashPrefix("ABCDEF".to_string()));
    }

    #[test]
    fn test_identifier_is_latest() {
        assert!(ChangeIdentifier::Latest.is_latest());
        assert!(!ChangeIdentifier::Sequence(0).is_latest());
        assert!(!ChangeIdentifier::HashPrefix("ABC".to_string()).is_latest());
    }

    #[test]
    fn test_identifier_is_sequence() {
        assert!(ChangeIdentifier::Sequence(42).is_sequence());
        assert!(!ChangeIdentifier::Latest.is_sequence());
        assert!(!ChangeIdentifier::HashPrefix("ABC".to_string()).is_sequence());
    }

    #[test]
    fn test_identifier_is_hash() {
        assert!(ChangeIdentifier::HashPrefix("ABC".to_string()).is_hash());
        let hash = Hash::of(b"test");
        assert!(ChangeIdentifier::FullHash(hash).is_hash());
        assert!(!ChangeIdentifier::Latest.is_hash());
        assert!(!ChangeIdentifier::Sequence(42).is_hash());
    }

    // =========================================================================
    // ChangeCmd Builder Tests
    // =========================================================================

    #[test]
    fn test_change_cmd_new() {
        let cmd = ChangeCmd::new();
        assert!(cmd.identifier.is_none());
        assert!(cmd.stack.is_none());
        assert_eq!(cmd.format, ChangeFormat::Default);
        assert!(!cmd.show_deps);
        assert!(!cmd.show_hunks);
        assert!(!cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_default() {
        let cmd = ChangeCmd::default();
        assert!(cmd.identifier.is_none());
        assert_eq!(cmd.format, ChangeFormat::Default);
    }

    #[test]
    fn test_change_cmd_with_identifier() {
        let cmd = ChangeCmd::new().with_identifier("ABCDEF");
        assert_eq!(cmd.identifier, Some("ABCDEF".to_string()));
    }

    #[test]
    fn test_change_cmd_with_identifier_string() {
        let cmd = ChangeCmd::new().with_identifier(String::from("12345"));
        assert_eq!(cmd.identifier, Some("12345".to_string()));
    }

    #[test]
    fn test_change_cmd_with_stack() {
        let cmd = ChangeCmd::new().with_stack("feature");
        assert_eq!(cmd.stack, Some("feature".to_string()));
    }

    #[test]
    fn test_change_cmd_with_format() {
        let cmd = ChangeCmd::new().with_format(ChangeFormat::Json);
        assert_eq!(cmd.format, ChangeFormat::Json);
    }

    #[test]
    fn test_change_cmd_with_show_deps() {
        let cmd = ChangeCmd::new().with_show_deps(true);
        assert!(cmd.show_deps);
    }

    #[test]
    fn test_change_cmd_with_show_hunks() {
        let cmd = ChangeCmd::new().with_show_hunks(true);
        assert!(cmd.show_hunks);
    }

    #[test]
    fn test_change_cmd_with_full_hash() {
        let cmd = ChangeCmd::new().with_full_hash(true);
        assert!(cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_builder_chain() {
        let cmd = ChangeCmd::new()
            .with_identifier("ABC123")
            .with_stack("main")
            .with_format(ChangeFormat::Short)
            .with_show_deps(true)
            .with_show_hunks(true)
            .with_full_hash(true);

        assert_eq!(cmd.identifier, Some("ABC123".to_string()));
        assert_eq!(cmd.stack, Some("main".to_string()));
        assert_eq!(cmd.format, ChangeFormat::Short);
        assert!(cmd.show_deps);
        assert!(cmd.show_hunks);
        assert!(cmd.full_hash);
    }

    #[test]
    fn test_change_cmd_get_hash_length_default() {
        let cmd = ChangeCmd::new();
        assert_eq!(cmd.get_hash_length(), DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_change_cmd_get_hash_length_full() {
        let cmd = ChangeCmd::new().with_full_hash(true);
        assert_eq!(cmd.get_hash_length(), 52);
    }

    // =========================================================================
    // JsonAuthor Tests
    // =========================================================================

    #[test]
    fn test_json_author_from_author_with_email() {
        let author = Author::new("Alice", Some("alice@example.com"));
        let json_author = JsonAuthor::from(&author);
        assert_eq!(json_author.name, "Alice");
        assert_eq!(json_author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_json_author_from_author_without_email() {
        let author = Author::new("Bob", None::<String>);
        let json_author = JsonAuthor::from(&author);
        assert_eq!(json_author.name, "Bob");
        assert!(json_author.email.is_none());
    }

    #[test]
    fn test_json_author_serialize() {
        let json_author = JsonAuthor {
            name: "Charlie".to_string(),
            email: Some("charlie@test.com".to_string()),
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Charlie\""));
        assert!(json.contains("\"email\":\"charlie@test.com\""));
    }

    #[test]
    fn test_json_author_serialize_no_email() {
        let json_author = JsonAuthor {
            name: "Dave".to_string(),
            email: None,
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Dave\""));
        // Email should be skipped
        assert!(!json.contains("email"));
    }

    // =========================================================================
    // JsonHunkSummary Tests
    // =========================================================================

    #[test]
    fn test_json_hunk_summary_with_path() {
        let summary = JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some("src/main.rs".to_string()),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"hunk_type\":\"FileAdd\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
    }

    #[test]
    fn test_json_hunk_summary_without_path() {
        let summary = JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"hunk_type\":\"Edit\""));
        assert!(!json.contains("path"));
    }

    // =========================================================================
    // JsonChange Tests
    // =========================================================================

    fn create_test_change() -> Change {
        Change::new(
            ChangeHeader::builder()
                .message("Test change message")
                .description("This is a description")
                .author(Author::new("Test User", Some("test@example.com")))
                .build(),
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn test_json_change_from_change() {
        let change = create_test_change();
        let hash = Hash::of(b"test change");
        let json_change = JsonChange::from_change(&change, &hash, Some(42));

        assert_eq!(json_change.message, "Test change message");
        assert_eq!(
            json_change.description,
            Some("This is a description".to_string())
        );
        assert_eq!(json_change.authors.len(), 1);
        assert_eq!(json_change.authors[0].name, "Test User");
        assert_eq!(json_change.sequence, Some(42));
        assert!(!json_change.has_provenance);
    }

    #[test]
    fn test_json_change_serialize() {
        let change = create_test_change();
        let hash = Hash::of(b"test change");
        let json_change = JsonChange::from_change(&change, &hash, None);

        let json = serde_json::to_string_pretty(&json_change).unwrap();
        assert!(json.contains("\"message\": \"Test change message\""));
        assert!(json.contains("\"description\": \"This is a description\""));
    }

    // =========================================================================
    // Helper Function Tests
    // =========================================================================

    #[test]
    fn test_truncate_string_no_truncation() {
        assert_eq!(truncate_string("Hello", 10), "Hello");
        assert_eq!(truncate_string("World", 5), "World");
    }

    #[test]
    fn test_truncate_string_exact_length() {
        assert_eq!(truncate_string("Hello", 5), "Hello");
    }

    #[test]
    fn test_truncate_string_with_ellipsis() {
        assert_eq!(truncate_string("Hello, World!", 8), "Hello...");
    }

    #[test]
    fn test_truncate_string_very_short_max() {
        assert_eq!(truncate_string("Hello", 3), "Hel");
        assert_eq!(truncate_string("Hello", 2), "He");
    }

    #[test]
    fn test_truncate_string_empty() {
        assert_eq!(truncate_string("", 5), "");
    }

    #[test]
    fn test_format_author_with_email() {
        let author = Author::new("Alice Smith", Some("alice@example.com"));
        assert_eq!(format_author(&author), "Alice Smith <alice@example.com>");
    }

    #[test]
    fn test_format_author_without_email() {
        let author = Author::new("Bob Jones", None::<String>);
        assert_eq!(format_author(&author), "Bob Jones");
    }

    // =========================================================================
    // Format Output Tests
    // =========================================================================

    #[test]
    fn test_format_short_basic() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, Some(5));

        assert!(output.contains("Test change message"));
        assert!(output.contains("#5"));
        assert!(output.contains("Test User"));
    }

    #[test]
    fn test_format_short_no_sequence() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        assert!(output.contains("Test change message"));
        assert!(!output.contains("#"));
    }

    #[test]
    fn test_format_json_basic() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_json(&change, &hash, Some(10));

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["message"], "Test change message");
        assert_eq!(parsed["sequence"], 10);
    }

    #[test]
    fn test_format_json_no_sequence() {
        let cmd = ChangeCmd::new();
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let output = cmd.format_json(&change, &hash, None);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.get("sequence").is_none() || parsed["sequence"].is_null());
    }

    // =========================================================================
    // Integration Tests
    // =========================================================================

    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TestGuard {
        original_dir: std::path::PathBuf,
        _temp_dir: TempDir,
    }

    impl TestGuard {
        fn new() -> Self {
            let original = env::current_dir().unwrap();
            let temp = TempDir::new().unwrap();
            env::set_current_dir(temp.path()).unwrap();
            Self {
                original_dir: original,
                _temp_dir: temp,
            }
        }
    }

    impl Drop for TestGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.original_dir);
        }
    }

    #[test]
    #[serial]
    fn test_change_run_outside_repository() {
        let _guard = TestGuard::new();

        let cmd = ChangeCmd::new();
        let result = cmd.run();

        assert!(result.is_err());
        match result {
            Err(CliError::RepositoryNotFound { .. }) => {}
            Err(CliError::Internal(_)) => {}
            _ => panic!("Expected RepositoryNotFound or Internal error"),
        }
    }

    #[test]
    #[serial]
    fn test_change_run_empty_repository() {
        let _guard = TestGuard::new();

        // Initialize empty repository
        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new();
        let result = cmd.run();

        // Should fail because no changes recorded
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_invalid_sequence() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_identifier("#999");
        let result = cmd.run();

        // Should fail with out of range
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_nonexistent_stack() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new()
            .with_identifier("#0")
            .with_stack("nonexistent");
        let result = cmd.run();

        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_json_format() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_format(ChangeFormat::Json);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_short_format() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_format(ChangeFormat::Short);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_with_show_deps() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_show_deps(true);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_change_run_with_show_hunks() {
        let _guard = TestGuard::new();

        let _repo = Repository::init(".").unwrap();

        let cmd = ChangeCmd::new().with_show_hunks(true);
        let result = cmd.run();

        // Will fail (no changes) but shouldn't panic
        assert!(result.is_err());
    }

    // =========================================================================
    // Debug and Clone Tests
    // =========================================================================

    #[test]
    fn test_change_format_debug() {
        let format = ChangeFormat::Default;
        let debug_str = format!("{:?}", format);
        assert_eq!(debug_str, "Default");
    }

    #[test]
    fn test_change_identifier_debug() {
        let id = ChangeIdentifier::Sequence(42);
        let debug_str = format!("{:?}", id);
        assert!(debug_str.contains("Sequence"));
        assert!(debug_str.contains("42"));
    }

    #[test]
    fn test_change_cmd_debug() {
        let cmd = ChangeCmd::new();
        let debug_str = format!("{:?}", cmd);
        assert!(debug_str.contains("ChangeCmd"));
    }

    #[test]
    fn test_change_cmd_clone() {
        let cmd = ChangeCmd::new()
            .with_identifier("ABC")
            .with_format(ChangeFormat::Json);
        let cloned = cmd.clone();

        assert_eq!(cmd.identifier, cloned.identifier);
        assert_eq!(cmd.format, cloned.format);
    }

    #[test]
    fn test_change_identifier_clone() {
        let id = ChangeIdentifier::HashPrefix("ABCD".to_string());
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    #[test]
    fn test_json_author_debug() {
        let author = JsonAuthor {
            name: "Test".to_string(),
            email: None,
        };
        let debug_str = format!("{:?}", author);
        assert!(debug_str.contains("JsonAuthor"));
    }

    #[test]
    fn test_json_hunk_summary_debug() {
        let summary = JsonHunkSummary {
            hunk_type: "FileAdd".to_string(),
            path: Some("test.rs".to_string()),
        };
        let debug_str = format!("{:?}", summary);
        assert!(debug_str.contains("JsonHunkSummary"));
    }

    #[test]
    fn test_json_change_debug() {
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let json_change = JsonChange::from_change(&change, &hash, None);
        let debug_str = format!("{:?}", json_change);
        assert!(debug_str.contains("JsonChange"));
    }

    #[test]
    fn test_json_author_clone() {
        let author = JsonAuthor {
            name: "Alice".to_string(),
            email: Some("alice@test.com".to_string()),
        };
        let cloned = author.clone();
        assert_eq!(author.name, cloned.name);
        assert_eq!(author.email, cloned.email);
    }

    #[test]
    fn test_json_hunk_summary_clone() {
        let summary = JsonHunkSummary {
            hunk_type: "Edit".to_string(),
            path: Some("file.rs".to_string()),
        };
        let cloned = summary.clone();
        assert_eq!(summary.hunk_type, cloned.hunk_type);
        assert_eq!(summary.path, cloned.path);
    }

    #[test]
    fn test_json_change_clone() {
        let change = create_test_change();
        let hash = Hash::of(b"test");
        let json_change = JsonChange::from_change(&change, &hash, Some(5));
        let cloned = json_change.clone();
        assert_eq!(json_change.hash, cloned.hash);
        assert_eq!(json_change.sequence, cloned.sequence);
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[test]
    fn test_identifier_parse_large_sequence() {
        let id = ChangeIdentifier::parse(Some("999999999999")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(999999999999));
    }

    #[test]
    fn test_identifier_parse_leading_zeros_numeric() {
        let id = ChangeIdentifier::parse(Some("007")).unwrap();
        assert_eq!(id, ChangeIdentifier::Sequence(7));
    }

    #[test]
    fn test_format_short_multiline_message() {
        let cmd = ChangeCmd::new();
        let change = Change::new(
            ChangeHeader::builder()
                .message("First line\nSecond line\nThird line")
                .author(Author::new("Test", None::<String>))
                .build(),
            vec![],
            vec![],
            vec![],
        );
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        // Short format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
    }

    #[test]
    fn test_format_short_no_authors() {
        let cmd = ChangeCmd::new();
        let change = Change::new(
            ChangeHeader::builder().message("No author message").build(),
            vec![],
            vec![],
            vec![],
        );
        let hash = Hash::of(b"test");
        let output = cmd.format_short(&change, &hash, None);

        assert!(output.contains("(unknown)"));
    }

    #[test]
    fn test_format_json_with_dependencies() {
        let cmd = ChangeCmd::new();
        let dep_hash = Hash::of(b"dependency");
        let change = Change::new(
            ChangeHeader::builder()
                .message("Change with dependency")
                .build(),
            vec![],
            vec![],
            vec![dep_hash],
        );
        let hash = Hash::of(b"main change");
        let output = cmd.format_json(&change, &hash, None);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed["dependencies"].is_array());
        assert_eq!(parsed["dependencies"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_count_unique_paths_empty() {
        let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
        assert_eq!(count_unique_paths(&hunks), 0);
    }

    #[test]
    fn test_truncate_string_unicode() {
        let result = truncate_string("Hello 世界!", 10);
        assert_eq!(result, "Hello 世界!");

        let result2 = truncate_string("Hello 世界!", 8);
        assert!(result2.ends_with("..."));
    }
}

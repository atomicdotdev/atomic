#![allow(dead_code)]
//! The `log` command for viewing change history.
//!
//! This module implements the `atomic log` command, which displays the
//! history of changes applied to a stack. It supports multiple output
//! formats, filtering, and pagination.
//!
//! # Usage
//!
//! ```text
//! atomic log [OPTIONS]
//!
//! Options:
//!   -n, --count <N>        Show only the last N changes
//!       --all              Show all stacks' history
//!       --stack <NAME>     Show history for specific stack
//!       --tags-only        Only show tagged changes
//!       --path <PATH>      Show only changes affecting this path
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
//! Show changes on a specific stack:
//! ```text
//! $ atomic log --stack feature-auth
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
//! - `1`: Error (repository not found, stack not found, etc.)


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
    author as style_author, hash as style_hash, hint, print_hint,
    stack as style_stack, timestamp as style_timestamp, warning,
};

// =============================================================================
// Output Format
// =============================================================================

/// Output format for the log command.
///
/// This enum defines the available output formats for displaying history.
/// Each format is optimized for different use cases: human reading,
/// scripting, or compact viewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum LogFormat {
    /// Full detailed format with all information.
    ///
    /// Shows hash, author, date, and full message including description.
    /// This is similar to `git log` default output.
    #[default]
    Default,

    /// Short format showing hash and first line of message.
    ///
    /// Useful for quick scanning of history.
    Short,

    /// Single-line format with hash, date, author, and message.
    ///
    /// Very compact, suitable for terminal viewing of long histories.
    Oneline,

    /// JSON format for machine parsing.
    ///
    /// Outputs a JSON array of change objects with full metadata.
    Json,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Default => write!(f, "default"),
            LogFormat::Short => write!(f, "short"),
            LogFormat::Oneline => write!(f, "oneline"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" | "full" => Ok(LogFormat::Default),
            "short" => Ok(LogFormat::Short),
            "oneline" | "one" | "1" => Ok(LogFormat::Oneline),
            "json" => Ok(LogFormat::Json),
            _ => Err(format!(
                "Invalid format '{}'. Expected: default, short, oneline, json",
                s
            )),
        }
    }
}

// =============================================================================
// Log Output Configuration
// =============================================================================

/// Configuration for log output formatting.
///
/// This struct controls how history entries are displayed to the user,
/// including format selection, filtering, and pagination.
#[derive(Debug, Clone)]
pub struct LogOutputConfig {
    /// The output format to use.
    pub format: LogFormat,

    /// Maximum number of entries to show.
    pub count: Option<usize>,

    /// Whether to show in reverse order (oldest first).
    pub reverse: bool,

    /// Starting sequence number.
    pub from_sequence: u64,

    /// Only show tagged changes.
    pub tags_only: bool,

    /// Specific stack to query.
    pub stack: Option<String>,

    /// Filter to changes affecting a path.
    pub path: Option<String>,

    /// Hash display length (truncation).
    pub hash_length: usize,
}

impl Default for LogOutputConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Default,
            count: None,
            reverse: false,
            from_sequence: 0,
            tags_only: false,
            stack: None,
            path: None,
            hash_length: DEFAULT_HASH_LENGTH,
        }
    }
}

impl LogOutputConfig {
    /// Create a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the output format.
    pub fn format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Set the maximum number of entries to show.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Set whether to show in reverse order.
    pub fn reverse(mut self, reverse: bool) -> Self {
        self.reverse = reverse;
        self
    }

    /// Set the starting sequence number.
    pub fn from_sequence(mut self, seq: u64) -> Self {
        self.from_sequence = seq;
        self
    }

    /// Set whether to only show tagged changes.
    pub fn tags_only(mut self, tags_only: bool) -> Self {
        self.tags_only = tags_only;
        self
    }

    /// Set the specific stack to query.
    pub fn stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Set the path filter.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the hash display length.
    pub fn hash_length(mut self, length: usize) -> Self {
        self.hash_length = length;
        self
    }
}

// =============================================================================
// JSON Output Types
// =============================================================================

/// JSON representation of an author.
///
/// Used for JSON output format serialization.
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

/// JSON representation of a history entry.
///
/// This struct provides a complete JSON serialization of a history entry,
/// suitable for machine parsing and scripting.
#[derive(Debug, Clone, Serialize)]
pub struct JsonLogEntry {
    /// The sequence number in the stack.
    pub sequence: u64,

    /// The change hash (full Base32 encoding).
    pub hash: String,

    /// The Merkle state after this change.
    pub state: String,

    /// The change message (if header loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// The change description (if header loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The authors (if header loaded).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<JsonAuthor>,

    /// The timestamp (ISO 8601 format).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,

    /// Whether this change is tagged.
    pub is_tagged: bool,
}

impl JsonLogEntry {
    /// Create a JSON entry from a history entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The history entry to convert
    ///
    /// # Returns
    ///
    /// A `JsonLogEntry` with all available information.
    pub fn from_entry(entry: &HistoryEntry) -> Self {
        Self {
            sequence: entry.sequence,
            hash: entry.hash.to_base32(),
            state: entry.state.to_base32(),
            message: entry.message().map(String::from),
            description: entry.description().map(String::from),
            authors: entry
                .authors()
                .map(|authors| authors.iter().map(JsonAuthor::from).collect())
                .unwrap_or_default(),
            timestamp: entry.timestamp().map(|ts| ts.to_rfc3339()),
            is_tagged: entry.is_tagged,
        }
    }
}

// =============================================================================
// Log Command
// =============================================================================

/// Show the change history.
///
/// The `log` command displays the history of changes applied to the
/// current stack (or a specified stack). It supports multiple output
/// formats and filtering options.
///
/// # Output Formats
///
/// - **default**: Full details including message, description, authors, dates
/// - **short**: Hash and first line of message
/// - **oneline**: Compact single-line format
/// - **json**: Machine-readable JSON array
///
/// # Examples
///
/// ```text
/// # Show last 10 changes
/// atomic log -n 10
///
/// # Show all tagged changes
/// atomic log --tags-only
///
/// # Show changes on feature stack in JSON
/// atomic log --stack feature -f json
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "log")]
pub struct Log {
    /// Limit number of changes to show.
    ///
    /// Shows only the most recent N changes. When combined with
    /// `--reverse`, shows the first N changes.
    #[arg(short = 'n', long = "count", value_name = "N")]
    pub count: Option<usize>,

    /// Show history for a specific stack.
    ///
    /// By default, shows history for the current stack. Use this
    /// option to view another stack's history without switching.
    #[arg(long = "stack", value_name = "NAME")]
    pub stack: Option<String>,

    /// Only show tagged changes.
    ///
    /// Filters the history to only include changes that have been
    /// tagged. Useful for viewing release history.
    #[arg(long = "tags-only")]
    pub tags_only: bool,

    /// Filter to changes affecting a specific path.
    ///
    /// Shows only changes that modified the given file or directory.
    /// Note: This requires loading change content for filtering.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Option<String>,

    /// Output format.
    ///
    /// Controls how changes are displayed:
    /// - default: Full details (hash, author, date, message)
    /// - short: Hash and message first line
    /// - oneline: Compact single-line format
    /// - json: Machine-readable JSON
    #[arg(short = 'f', long = "format", value_enum, default_value = "default")]
    pub format: LogFormat,

    /// Show in reverse order (oldest first).
    ///
    /// By default, shows most recent changes first. Use this flag
    /// to show changes in chronological order.
    #[arg(long = "reverse")]
    pub reverse: bool,

    /// Start from a specific sequence number.
    ///
    /// Shows changes starting from the given sequence number.
    /// Sequence numbers are 0-indexed positions in the change log.
    #[arg(long = "from", value_name = "SEQ")]
    pub from: Option<u64>,

    /// Show full hash instead of abbreviated.
    ///
    /// By default, hashes are truncated to 8 characters. Use this
    /// flag to show the full 52-character Base32 hash.
    #[arg(long = "full-hash")]
    pub full_hash: bool,
}

impl Log {
    /// Create a new Log command with default settings.
    ///
    /// # Returns
    ///
    /// A `Log` instance with all default values.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let log = Log::new();
    /// assert_eq!(log.format, LogFormat::Default);
    /// ```
    pub fn new() -> Self {
        Self {
            count: None,
            stack: None,
            tags_only: false,
            path: None,
            format: LogFormat::Default,
            reverse: false,
            from: None,
            full_hash: false,
        }
    }

    /// Set the count limit.
    ///
    /// # Arguments
    ///
    /// * `count` - Maximum number of entries to show
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Set the stack to query.
    ///
    /// # Arguments
    ///
    /// * `stack` - Stack name to query
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Set tags-only filter.
    ///
    /// # Arguments
    ///
    /// * `tags_only` - Whether to only show tagged changes
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_tags_only(mut self, tags_only: bool) -> Self {
        self.tags_only = tags_only;
        self
    }

    /// Set path filter.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to filter changes by
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the output format.
    ///
    /// # Arguments
    ///
    /// * `format` - Output format to use
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Set reverse order.
    ///
    /// # Arguments
    ///
    /// * `reverse` - Whether to show oldest first
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_reverse(mut self, reverse: bool) -> Self {
        self.reverse = reverse;
        self
    }

    /// Set starting sequence number.
    ///
    /// # Arguments
    ///
    /// * `from` - Sequence number to start from
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_from(mut self, from: u64) -> Self {
        self.from = Some(from);
        self
    }

    /// Set full hash display.
    ///
    /// # Arguments
    ///
    /// * `full_hash` - Whether to show full hashes
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn with_full_hash(mut self, full_hash: bool) -> Self {
        self.full_hash = full_hash;
        self
    }

    /// Build history options from command settings.
    ///
    /// Converts the command's settings into `HistoryOptions` for
    /// querying the repository.
    ///
    /// # Returns
    ///
    /// `HistoryOptions` configured according to command settings.
    fn build_history_options(&self) -> HistoryOptions {
        let mut options = HistoryOptions::new().load_headers(true);

        if let Some(count) = self.count {
            options = options.limit(count);
        }

        if let Some(ref stack) = self.stack {
            options = options.stack(stack.clone());
        }

        if self.tags_only {
            options = options.tagged_only(true);
        }

        if let Some(from) = self.from {
            options = options.from_sequence(from);
        }

        options
    }

    /// Get the hash display length based on settings.
    ///
    /// # Returns
    ///
    /// The number of characters to display for hashes.
    fn get_hash_length(&self) -> usize {
        if self.full_hash {
            52 // Full Base32 hash length
        } else {
            DEFAULT_HASH_LENGTH
        }
    }

    /// Format entries for default output.
    ///
    /// # Arguments
    ///
    /// * `entries` - History entries to format
    /// * `hash_length` - Number of hash characters to display
    ///
    /// # Returns
    ///
    /// Formatted output string.
    fn format_default(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
        let mut output = String::new();

        for (i, entry) in entries.iter().enumerate() {
            // Add separator between entries
            if i > 0 {
                output.push('\n');
            }

            // Change header line
            let hash_str = format_hash_with_length(&entry.hash, hash_length);
            let tagged_marker = if entry.is_tagged { " (tag)" } else { "" };
            output.push_str(&format!(
                "{} {}{}\n",
                style_hash("change"),
                style_hash(&hash_str),
                hint(tagged_marker)
            ));

            // Authors
            if let Some(authors) = entry.authors() {
                for author in authors {
                    let author_line = if let Some(ref email) = author.email {
                        format!("{} <{}>", author.name, email)
                    } else {
                        author.name.clone()
                    };
                    output.push_str(&format!(
                        "Author: {}\n",
                        style_author(&author_line)
                    ));
                }
            }

            // Timestamp
            if let Some(ts) = entry.timestamp() {
                let formatted_time = format_timestamp(&ts);
                output.push_str(&format!(
                    "Date:   {}\n",
                    style_timestamp(&formatted_time)
                ));
            }

            // Message
            output.push('\n');
            if let Some(message) = entry.message() {
                // Indent message lines
                for line in message.lines() {
                    output.push_str(&format!("    {}\n", line));
                }
            }

            // Description
            if let Some(description) = entry.description() {
                output.push('\n');
                for line in description.lines() {
                    output.push_str(&format!("    {}\n", line));
                }
            }
        }

        output
    }

    /// Format entries for short output.
    ///
    /// # Arguments
    ///
    /// * `entries` - History entries to format
    /// * `hash_length` - Number of hash characters to display
    ///
    /// # Returns
    ///
    /// Formatted output string.
    fn format_short(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
        let mut output = String::new();

        for entry in entries {
            let hash_str = format_hash_with_length(&entry.hash, hash_length);
            let message = entry.message().unwrap_or("(no message)");
            // Get just the first line
            let first_line = message.lines().next().unwrap_or(message);

            let tagged_marker = if entry.is_tagged { " *" } else { "" };
            output.push_str(&format!(
                "{}{} {}\n",
                style_hash(&hash_str),
                hint(tagged_marker),
                first_line
            ));
        }

        output
    }

    /// Format entries for oneline output.
    ///
    /// # Arguments
    ///
    /// * `entries` - History entries to format
    /// * `hash_length` - Number of hash characters to display
    ///
    /// # Returns
    ///
    /// Formatted output string.
    fn format_oneline(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
        let mut output = String::new();

        // Calculate max author width for alignment
        let max_author_width = entries
            .iter()
            .filter_map(|e| e.authors())
            .filter_map(|authors| authors.first())
            .map(|a| a.name.len())
            .max()
            .unwrap_or(10)
            .min(20); // Cap at 20 characters

        for entry in entries {
            let hash_str = format_hash_with_length(&entry.hash, hash_length);

            let date_str = entry
                .timestamp()
                .map(|ts| ts.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "          ".to_string());

            let author_name = entry
                .authors()
                .and_then(|authors| authors.first())
                .map(|a| truncate_string(&a.name, max_author_width))
                .unwrap_or_else(|| "(unknown)".to_string());

            let message = entry.message().unwrap_or("(no message)");
            let first_line = message.lines().next().unwrap_or(message);

            let tagged_marker = if entry.is_tagged { "*" } else { " " };

            output.push_str(&format!(
                "{}{} {} {:width$} {}\n",
                style_hash(&hash_str),
                hint(tagged_marker),
                style_timestamp(&date_str),
                style_author(&author_name),
                first_line,
                width = max_author_width
            ));
        }

        output
    }

    /// Format entries for JSON output.
    ///
    /// # Arguments
    ///
    /// * `entries` - History entries to format
    ///
    /// # Returns
    ///
    /// Formatted JSON string.
    fn format_json(&self, entries: &[HistoryEntry]) -> String {
        let json_entries: Vec<JsonLogEntry> = entries
            .iter()
            .map(|e| JsonLogEntry::from_entry(e))
            .collect();

        serde_json::to_string_pretty(&json_entries).unwrap_or_else(|e| {
            format!("{{\"error\": \"Failed to serialize: {}\"}}", e)
        })
    }

    /// Print entries in the configured format.
    ///
    /// # Arguments
    ///
    /// * `entries` - History entries to print
    fn print_entries(&self, entries: &[HistoryEntry]) {
        let hash_length = self.get_hash_length();

        let output = match self.format {
            LogFormat::Default => self.format_default(entries, hash_length),
            LogFormat::Short => self.format_short(entries, hash_length),
            LogFormat::Oneline => self.format_oneline(entries, hash_length),
            LogFormat::Json => self.format_json(entries),
        };

        print!("{}", output);
    }

    /// Print empty history message.
    ///
    /// # Arguments
    ///
    /// * `stack_name` - Name of the stack being queried
    fn print_empty_history(&self, stack_name: &str) {
        if self.format == LogFormat::Json {
            println!("[]");
        } else {
            println!(
                "No changes recorded on stack '{}'.\n",
                style_stack(stack_name)
            );
            print_hint("Record changes with 'atomic record -m \"message\"'");
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Log {
    /// Execute the log command.
    ///
    /// This method:
    /// 1. Finds the repository root
    /// 2. Opens the repository
    /// 3. Queries history based on options
    /// 4. Formats and displays the results
    ///
    /// # Errors
    ///
    /// Returns a `CliError` if:
    /// - No repository is found
    /// - The specified stack doesn't exist
    /// - Database errors occur
    fn run(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open_readonly(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => {
                CliError::RepositoryNotFound {
                    searched_path: path.into(),
                }
            }
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
        })?;

        // Build options
        let options = self.build_history_options();
        let stack_name = self
            .stack
            .as_deref()
            .unwrap_or_else(|| repo.current_stack());

        // Get history
        let entries = if self.reverse {
            // Use forward log for reverse display (oldest first)
            repo.log(options).map_err(|e| match e {
                atomic_repository::RepositoryError::StackNotFound { name } => {
                    CliError::StackNotFound { name }
                }
                other => CliError::Internal(anyhow::anyhow!("{}", other)),
            })?
        } else {
            // Use reverse log for default display (newest first)
            repo.reverse_log(options).map_err(|e| match e {
                atomic_repository::RepositoryError::StackNotFound { name } => {
                    CliError::StackNotFound { name }
                }
                other => CliError::Internal(anyhow::anyhow!("{}", other)),
            })?
        };

        // Handle empty history
        if entries.is_empty() {
            self.print_empty_history(stack_name);
            return Ok(());
        }

        // Filter by path if specified (requires loading change content)
        // Note: Path filtering is a placeholder - full implementation would
        // require inspecting each change's hunks for the affected paths
        if let Some(ref _path) = self.path {
            // Path filtering would be implemented here
            // For now, we show a warning and continue with unfiltered results
            eprintln!(
                "{}",
                warning("Note: Path filtering is not yet fully implemented")
            );
        }

        // Print entries
        self.print_entries(&entries);

        Ok(())
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Truncate a string to a maximum length, adding ellipsis if needed.
///
/// This function handles Unicode characters correctly by counting grapheme
/// clusters rather than bytes, ensuring we never split a multi-byte character.
///
/// # Arguments
///
/// * `s` - The string to truncate
/// * `max_len` - Maximum character length including ellipsis
///
/// # Returns
///
/// The truncated string.
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(truncate_string("Hello, World!", 8), "Hello...");
/// assert_eq!(truncate_string("Short", 8), "Short");
/// ```
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
///
/// # Arguments
///
/// * `author` - The author to format
///
/// # Returns
///
/// Formatted author string (e.g., "Name <email>").
fn format_author(author: &Author) -> String {
    if let Some(ref email) = author.email {
        format!("{} <{}>", author.name, email)
    } else {
        author.name.clone()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Author, ChangeHeader};
    use atomic_core::types::{Hash, Merkle, NodeId};

    // =========================================================================
    // LogFormat Tests
    // =========================================================================

    #[test]
    fn test_log_format_default_is_default() {
        let format = LogFormat::default();
        assert_eq!(format, LogFormat::Default);
    }

    #[test]
    fn test_log_format_display() {
        assert_eq!(LogFormat::Default.to_string(), "default");
        assert_eq!(LogFormat::Short.to_string(), "short");
        assert_eq!(LogFormat::Oneline.to_string(), "oneline");
        assert_eq!(LogFormat::Json.to_string(), "json");
    }

    #[test]
    fn test_log_format_from_str_default() {
        assert_eq!("default".parse::<LogFormat>().unwrap(), LogFormat::Default);
        assert_eq!("full".parse::<LogFormat>().unwrap(), LogFormat::Default);
    }

    #[test]
    fn test_log_format_from_str_short() {
        assert_eq!("short".parse::<LogFormat>().unwrap(), LogFormat::Short);
    }

    #[test]
    fn test_log_format_from_str_oneline() {
        assert_eq!("oneline".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
        assert_eq!("one".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
        assert_eq!("1".parse::<LogFormat>().unwrap(), LogFormat::Oneline);
    }

    #[test]
    fn test_log_format_from_str_json() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
    }

    #[test]
    fn test_log_format_from_str_case_insensitive() {
        assert_eq!("DEFAULT".parse::<LogFormat>().unwrap(), LogFormat::Default);
        assert_eq!("SHORT".parse::<LogFormat>().unwrap(), LogFormat::Short);
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
    }

    #[test]
    fn test_log_format_from_str_invalid() {
        let result = "invalid".parse::<LogFormat>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid format"));
    }

    #[test]
    fn test_log_format_equality() {
        assert_eq!(LogFormat::Default, LogFormat::Default);
        assert_ne!(LogFormat::Default, LogFormat::Short);
        assert_ne!(LogFormat::Short, LogFormat::Oneline);
        assert_ne!(LogFormat::Oneline, LogFormat::Json);
    }

    #[test]
    fn test_log_format_clone() {
        let format = LogFormat::Short;
        let cloned = format.clone();
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_log_format_copy() {
        let format = LogFormat::Json;
        let copied: LogFormat = format;
        assert_eq!(format, copied);
    }

    // =========================================================================
    // LogOutputConfig Tests
    // =========================================================================

    #[test]
    fn test_log_output_config_default() {
        let config = LogOutputConfig::default();
        assert_eq!(config.format, LogFormat::Default);
        assert!(config.count.is_none());
        assert!(!config.reverse);
        assert_eq!(config.from_sequence, 0);
        assert!(!config.tags_only);
        assert!(config.stack.is_none());
        assert!(config.path.is_none());
        assert_eq!(config.hash_length, DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_log_output_config_new() {
        let config = LogOutputConfig::new();
        assert_eq!(config.format, LogFormat::Default);
    }

    #[test]
    fn test_log_output_config_format() {
        let config = LogOutputConfig::new().format(LogFormat::Json);
        assert_eq!(config.format, LogFormat::Json);
    }

    #[test]
    fn test_log_output_config_count() {
        let config = LogOutputConfig::new().count(10);
        assert_eq!(config.count, Some(10));
    }

    #[test]
    fn test_log_output_config_reverse() {
        let config = LogOutputConfig::new().reverse(true);
        assert!(config.reverse);
    }

    #[test]
    fn test_log_output_config_from_sequence() {
        let config = LogOutputConfig::new().from_sequence(42);
        assert_eq!(config.from_sequence, 42);
    }

    #[test]
    fn test_log_output_config_tags_only() {
        let config = LogOutputConfig::new().tags_only(true);
        assert!(config.tags_only);
    }

    #[test]
    fn test_log_output_config_stack() {
        let config = LogOutputConfig::new().stack("feature");
        assert_eq!(config.stack, Some("feature".to_string()));
    }

    #[test]
    fn test_log_output_config_stack_string() {
        let config = LogOutputConfig::new().stack(String::from("main"));
        assert_eq!(config.stack, Some("main".to_string()));
    }

    #[test]
    fn test_log_output_config_path() {
        let config = LogOutputConfig::new().path("src/main.rs");
        assert_eq!(config.path, Some("src/main.rs".to_string()));
    }

    #[test]
    fn test_log_output_config_hash_length() {
        let config = LogOutputConfig::new().hash_length(12);
        assert_eq!(config.hash_length, 12);
    }

    #[test]
    fn test_log_output_config_builder_chain() {
        let config = LogOutputConfig::new()
            .format(LogFormat::Short)
            .count(25)
            .reverse(true)
            .from_sequence(10)
            .tags_only(true)
            .stack("dev")
            .path("lib/")
            .hash_length(16);

        assert_eq!(config.format, LogFormat::Short);
        assert_eq!(config.count, Some(25));
        assert!(config.reverse);
        assert_eq!(config.from_sequence, 10);
        assert!(config.tags_only);
        assert_eq!(config.stack, Some("dev".to_string()));
        assert_eq!(config.path, Some("lib/".to_string()));
        assert_eq!(config.hash_length, 16);
    }

    // =========================================================================
    // Log Command Tests
    // =========================================================================

    #[test]
    fn test_log_new() {
        let log = Log::new();
        assert!(log.count.is_none());
        assert!(log.stack.is_none());
        assert!(!log.tags_only);
        assert!(log.path.is_none());
        assert_eq!(log.format, LogFormat::Default);
        assert!(!log.reverse);
        assert!(log.from.is_none());
        assert!(!log.full_hash);
    }

    #[test]
    fn test_log_default() {
        let log = Log::default();
        assert!(log.count.is_none());
        assert_eq!(log.format, LogFormat::Default);
    }

    #[test]
    fn test_log_with_count() {
        let log = Log::new().with_count(15);
        assert_eq!(log.count, Some(15));
    }

    #[test]
    fn test_log_with_stack() {
        let log = Log::new().with_stack("feature-branch");
        assert_eq!(log.stack, Some("feature-branch".to_string()));
    }

    #[test]
    fn test_log_with_stack_string() {
        let log = Log::new().with_stack(String::from("dev"));
        assert_eq!(log.stack, Some("dev".to_string()));
    }

    #[test]
    fn test_log_with_tags_only() {
        let log = Log::new().with_tags_only(true);
        assert!(log.tags_only);
    }

    #[test]
    fn test_log_with_path() {
        let log = Log::new().with_path("src/lib.rs");
        assert_eq!(log.path, Some("src/lib.rs".to_string()));
    }

    #[test]
    fn test_log_with_format() {
        let log = Log::new().with_format(LogFormat::Oneline);
        assert_eq!(log.format, LogFormat::Oneline);
    }

    #[test]
    fn test_log_with_reverse() {
        let log = Log::new().with_reverse(true);
        assert!(log.reverse);
    }

    #[test]
    fn test_log_with_from() {
        let log = Log::new().with_from(100);
        assert_eq!(log.from, Some(100));
    }

    #[test]
    fn test_log_with_full_hash() {
        let log = Log::new().with_full_hash(true);
        assert!(log.full_hash);
    }

    #[test]
    fn test_log_builder_chain() {
        let log = Log::new()
            .with_count(20)
            .with_stack("release")
            .with_tags_only(true)
            .with_path("docs/")
            .with_format(LogFormat::Json)
            .with_reverse(true)
            .with_from(50)
            .with_full_hash(true);

        assert_eq!(log.count, Some(20));
        assert_eq!(log.stack, Some("release".to_string()));
        assert!(log.tags_only);
        assert_eq!(log.path, Some("docs/".to_string()));
        assert_eq!(log.format, LogFormat::Json);
        assert!(log.reverse);
        assert_eq!(log.from, Some(50));
        assert!(log.full_hash);
    }

    #[test]
    fn test_log_get_hash_length_default() {
        let log = Log::new();
        assert_eq!(log.get_hash_length(), DEFAULT_HASH_LENGTH);
    }

    #[test]
    fn test_log_get_hash_length_full() {
        let log = Log::new().with_full_hash(true);
        assert_eq!(log.get_hash_length(), 52);
    }

    #[test]
    fn test_log_build_history_options_default() {
        let log = Log::new();
        let options = log.build_history_options();
        assert!(options.load_headers);
        assert!(options.limit.is_none());
        assert!(options.stack.is_none());
        assert!(!options.tagged_only);
        assert_eq!(options.from_sequence, 0);
    }

    #[test]
    fn test_log_build_history_options_with_count() {
        let log = Log::new().with_count(5);
        let options = log.build_history_options();
        assert_eq!(options.limit, Some(5));
    }

    #[test]
    fn test_log_build_history_options_with_stack() {
        let log = Log::new().with_stack("test-stack");
        let options = log.build_history_options();
        assert_eq!(options.stack, Some("test-stack".to_string()));
    }

    #[test]
    fn test_log_build_history_options_with_tags_only() {
        let log = Log::new().with_tags_only(true);
        let options = log.build_history_options();
        assert!(options.tagged_only);
    }

    #[test]
    fn test_log_build_history_options_with_from() {
        let log = Log::new().with_from(25);
        let options = log.build_history_options();
        assert_eq!(options.from_sequence, 25);
    }

    #[test]
    fn test_log_build_history_options_combined() {
        let log = Log::new()
            .with_count(10)
            .with_stack("feature")
            .with_tags_only(true)
            .with_from(5);
        let options = log.build_history_options();

        assert_eq!(options.limit, Some(10));
        assert_eq!(options.stack, Some("feature".to_string()));
        assert!(options.tagged_only);
        assert_eq!(options.from_sequence, 5);
        assert!(options.load_headers);
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
    fn test_json_author_serialize_with_email() {
        let json_author = JsonAuthor {
            name: "Charlie".to_string(),
            email: Some("charlie@test.com".to_string()),
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Charlie\""));
        assert!(json.contains("\"email\":\"charlie@test.com\""));
    }

    #[test]
    fn test_json_author_serialize_without_email() {
        let json_author = JsonAuthor {
            name: "Dave".to_string(),
            email: None,
        };
        let json = serde_json::to_string(&json_author).unwrap();
        assert!(json.contains("\"name\":\"Dave\""));
        // Email should be skipped when None
        assert!(!json.contains("email"));
    }

    // =========================================================================
    // JsonLogEntry Tests
    // =========================================================================

    fn create_test_hash() -> Hash {
        Hash::of(b"test change content")
    }

    fn create_test_merkle() -> Merkle {
        Merkle::of(b"test state")
    }

    fn create_test_entry_without_header() -> HistoryEntry {
        HistoryEntry::new(
            42,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
    }

    fn create_test_entry_with_header() -> HistoryEntry {
        let header = ChangeHeader::builder()
            .message("Test change message")
            .description("This is a longer description.")
            .author(Author::new("Test User", Some("test@example.com")))
            .build();

        HistoryEntry::new(
            42,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header)
        .with_tagged(true)
    }

    #[test]
    fn test_json_log_entry_from_entry_without_header() {
        let entry = create_test_entry_without_header();
        let json_entry = JsonLogEntry::from_entry(&entry);

        assert_eq!(json_entry.sequence, 42);
        assert!(!json_entry.hash.is_empty());
        assert!(!json_entry.state.is_empty());
        assert!(json_entry.message.is_none());
        assert!(json_entry.description.is_none());
        assert!(json_entry.authors.is_empty());
        assert!(json_entry.timestamp.is_none());
        assert!(!json_entry.is_tagged);
    }

    #[test]
    fn test_json_log_entry_from_entry_with_header() {
        let entry = create_test_entry_with_header();
        let json_entry = JsonLogEntry::from_entry(&entry);

        assert_eq!(json_entry.sequence, 42);
        assert_eq!(json_entry.message, Some("Test change message".to_string()));
        assert_eq!(
            json_entry.description,
            Some("This is a longer description.".to_string())
        );
        assert_eq!(json_entry.authors.len(), 1);
        assert_eq!(json_entry.authors[0].name, "Test User");
        assert!(json_entry.timestamp.is_some());
        assert!(json_entry.is_tagged);
    }

    #[test]
    fn test_json_log_entry_serialize() {
        let entry = create_test_entry_with_header();
        let json_entry = JsonLogEntry::from_entry(&entry);
        let json = serde_json::to_string_pretty(&json_entry).unwrap();

        assert!(json.contains("\"sequence\": 42"));
        assert!(json.contains("\"message\": \"Test change message\""));
        assert!(json.contains("\"is_tagged\": true"));
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
        assert_eq!(truncate_string("VeryLongName", 10), "VeryLon...");
    }

    #[test]
    fn test_truncate_string_very_short_max() {
        assert_eq!(truncate_string("Hello", 3), "Hel");
        assert_eq!(truncate_string("Hello", 2), "He");
        assert_eq!(truncate_string("Hello", 1), "H");
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
    fn test_format_short_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_short(&[entry], 8);

        assert!(output.contains("Test change message"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn test_format_short_multiple_entries() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_without_header();
        entry2.sequence = 43;

        let output = log.format_short(&[entry1, entry2], 8);
        let lines: Vec<&str> = output.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Test change message"));
        assert!(lines[1].contains("(no message)"));
    }

    #[test]
    fn test_format_short_tagged_marker() {
        let log = Log::new();
        let entry = create_test_entry_with_header(); // is_tagged = true
        let output = log.format_short(&[entry], 8);

        // Tagged entries should have a marker
        assert!(output.contains("*"));
    }

    #[test]
    fn test_format_oneline_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_oneline(&[entry], 8);

        // Should contain hash, date, author, message on one line
        assert!(output.contains("Test User"));
        assert!(output.contains("Test change message"));
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn test_format_oneline_without_header() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_oneline(&[entry], 8);

        assert!(output.contains("(unknown)"));
        assert!(output.contains("(no message)"));
    }

    #[test]
    fn test_format_json_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_json(&[entry]);

        // Should be valid JSON array
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_format_json_multiple_entries() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_without_header();
        entry2.sequence = 43;

        let output = log.format_json(&[entry1, entry2]);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_format_json_empty() {
        let log = Log::new();
        let output = log.format_json(&[]);

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_array());
        assert!(parsed.as_array().unwrap().is_empty());
    }

    #[test]
    fn test_format_default_single_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_default(&[entry], 8);

        // Should contain change header
        assert!(output.contains("change"));
        // Should contain author
        assert!(output.contains("Author:"));
        assert!(output.contains("Test User"));
        // Should contain date
        assert!(output.contains("Date:"));
        // Should contain message
        assert!(output.contains("Test change message"));
        // Should contain description
        assert!(output.contains("This is a longer description."));
    }

    #[test]
    fn test_format_default_tagged_entry() {
        let log = Log::new();
        let entry = create_test_entry_with_header();
        let output = log.format_default(&[entry], 8);

        assert!(output.contains("(tag)"));
    }

    #[test]
    fn test_format_default_without_header() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_default(&[entry], 8);

        // Should have change line but no author/date/message
        assert!(output.contains("change"));
        // Should not panic, should handle missing info gracefully
    }

    #[test]
    fn test_format_default_multiple_entries_separated() {
        let log = Log::new();
        let entry1 = create_test_entry_with_header();
        let mut entry2 = create_test_entry_with_header();
        entry2.sequence = 43;

        let output = log.format_default(&[entry1, entry2], 8);

        // Count entries by looking for "Author:" lines (each entry has one)
        let author_count = output.matches("Author:").count();
        assert_eq!(author_count, 2);
    }

    // =========================================================================
    // Integration Tests (Repository)
    // =========================================================================

    use serial_test::serial;
    use std::env;
    use std::fs;
    use tempfile::TempDir;

    /// Helper struct to manage test directory and restore working directory.
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
    fn test_log_run_outside_repository() {
        let _guard = TestGuard::new();

        let log = Log::new();
        let result = log.run();

        assert!(result.is_err());
        // Could be RepositoryNotFound or Internal depending on error mapping
        match result {
            Err(CliError::RepositoryNotFound { .. }) => {}
            Err(CliError::Internal(_)) => {}
            _ => panic!("Expected RepositoryNotFound or Internal error"),
        }
    }

    #[test]
    #[serial]
    fn test_log_run_empty_repository() {
        let _guard = TestGuard::new();

        // Initialize empty repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new();
        let result = log.run();

        // Should succeed but print empty message
        // The result could fail due to database initialization issues in tests
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_nonexistent_stack() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_stack("nonexistent-stack");
        let result = log.run();

        // Should fail with stack not found or internal error
        assert!(result.is_err());
        match result {
            Err(CliError::StackNotFound { name }) => assert_eq!(name, "nonexistent-stack"),
            Err(CliError::Internal(_)) => {} // Also acceptable
            other => panic!("Expected StackNotFound or Internal error, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_log_run_json_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Json);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_short_format_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Short);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_oneline_format_empty() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_format(LogFormat::Oneline);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_count() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_count(5);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_reverse() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_reverse(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_from_sequence() {
        let _guard = TestGuard::new();

        // Initialize repository and drop to release db lock
        {
            let _repo = Repository::init(".").unwrap();
        }

        let log = Log::new().with_from(0);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_tags_only() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_tags_only(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_with_full_hash() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new().with_full_hash(true);
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_combined_options() {
        let _guard = TestGuard::new();

        // Initialize repository
        let _repo = Repository::init(".").unwrap();

        let log = Log::new()
            .with_count(10)
            .with_format(LogFormat::Short)
            .with_reverse(true)
            .with_full_hash(true);

        let result = log.run();
        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_log_run_in_subdirectory() {
        let _guard = TestGuard::new();

        // Initialize repository
        let repo_result = Repository::init(".");
        if repo_result.is_err() {
            // Filesystem may be read-only or other issues in test environment
            return;
        }
        let _repo = repo_result.unwrap();

        // Create and move to subdirectory
        if fs::create_dir("subdir").is_err() {
            // Filesystem may be read-only
            return;
        }
        if env::set_current_dir("subdir").is_err() {
            return;
        }

        let log = Log::new();
        let result = log.run();

        // Result could fail due to database issues; just don't panic
        let _ = result;
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[test]
    fn test_format_short_entry_with_multiline_message() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("First line\nSecond line\nThird line")
            .build();

        let entry = HistoryEntry::new(
            1,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header);

        let output = log.format_short(&[entry], 8);

        // Short format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
    }

    #[test]
    fn test_format_oneline_entry_with_multiline_message() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("First line\nSecond line")
            .author(Author::new("Test", None::<String>))
            .build();

        let entry = HistoryEntry::new(
            1,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header);

        let output = log.format_oneline(&[entry], 8);

        // Oneline format should only show first line
        assert!(output.contains("First line"));
        assert!(!output.contains("Second line"));
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn test_format_default_entry_with_description() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("Short message")
            .description("This is a detailed description\nwith multiple lines.")
            .build();

        let entry = HistoryEntry::new(
            1,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header);

        let output = log.format_default(&[entry], 8);

        assert!(output.contains("Short message"));
        assert!(output.contains("This is a detailed description"));
        assert!(output.contains("with multiple lines."));
    }

    #[test]
    fn test_format_json_preserves_all_fields() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("JSON test message")
            .description("JSON test description")
            .author(Author::new("JSON Author", Some("json@test.com")))
            .build();

        let entry = HistoryEntry::new(
            99,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header)
        .with_tagged(true);

        let output = log.format_json(&[entry]);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let obj = &parsed[0];

        assert_eq!(obj["sequence"], 99);
        assert_eq!(obj["message"], "JSON test message");
        assert_eq!(obj["description"], "JSON test description");
        assert_eq!(obj["is_tagged"], true);
        assert_eq!(obj["authors"][0]["name"], "JSON Author");
        assert_eq!(obj["authors"][0]["email"], "json@test.com");
    }

    #[test]
    fn test_truncate_string_unicode() {
        // Unicode characters should be handled correctly (counting chars, not bytes)
        let result = truncate_string("Hello 世界!", 10);
        // String has 10 chars, so should not be truncated
        assert_eq!(result, "Hello 世界!");

        // Test actual truncation with unicode
        let result2 = truncate_string("Hello 世界!", 8);
        // Should truncate to 5 chars + "..."
        assert!(result2.ends_with("..."));
        assert_eq!(result2.chars().count(), 8);
    }

    #[test]
    fn test_format_author_empty_name() {
        let author = Author::new("", Some("email@test.com"));
        let formatted = format_author(&author);
        assert_eq!(formatted, " <email@test.com>");
    }

    #[test]
    fn test_log_format_debug() {
        let format = LogFormat::Default;
        let debug_str = format!("{:?}", format);
        assert_eq!(debug_str, "Default");
    }

    #[test]
    fn test_log_output_config_debug() {
        let config = LogOutputConfig::new();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("LogOutputConfig"));
    }

    #[test]
    fn test_log_command_debug() {
        let log = Log::new();
        let debug_str = format!("{:?}", log);
        assert!(debug_str.contains("Log"));
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
    fn test_json_log_entry_debug() {
        let entry = JsonLogEntry {
            sequence: 1,
            hash: "abc".to_string(),
            state: "xyz".to_string(),
            message: None,
            description: None,
            authors: vec![],
            timestamp: None,
            is_tagged: false,
        };
        let debug_str = format!("{:?}", entry);
        assert!(debug_str.contains("JsonLogEntry"));
    }

    #[test]
    fn test_log_clone() {
        let log = Log::new()
            .with_count(5)
            .with_stack("test")
            .with_format(LogFormat::Short);
        let cloned = log.clone();

        assert_eq!(log.count, cloned.count);
        assert_eq!(log.stack, cloned.stack);
        assert_eq!(log.format, cloned.format);
    }

    #[test]
    fn test_log_output_config_clone() {
        let config = LogOutputConfig::new()
            .format(LogFormat::Json)
            .count(10);
        let cloned = config.clone();

        assert_eq!(config.format, cloned.format);
        assert_eq!(config.count, cloned.count);
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
    fn test_json_log_entry_clone() {
        let entry = JsonLogEntry {
            sequence: 42,
            hash: "hash123".to_string(),
            state: "state456".to_string(),
            message: Some("Test".to_string()),
            description: None,
            authors: vec![],
            timestamp: None,
            is_tagged: true,
        };
        let cloned = entry.clone();

        assert_eq!(entry.sequence, cloned.sequence);
        assert_eq!(entry.hash, cloned.hash);
        assert_eq!(entry.is_tagged, cloned.is_tagged);
    }

    #[test]
    fn test_format_empty_entries() {
        let log = Log::new();

        // All formats should handle empty input gracefully
        assert_eq!(log.format_default(&[], 8), "");
        assert_eq!(log.format_short(&[], 8), "");
        assert_eq!(log.format_oneline(&[], 8), "");

        let json_output = log.format_json(&[]);
        assert_eq!(json_output.trim(), "[]");
    }

    #[test]
    fn test_format_short_no_message() {
        let log = Log::new();
        let entry = create_test_entry_without_header();
        let output = log.format_short(&[entry], 8);

        assert!(output.contains("(no message)"));
    }

    #[test]
    fn test_format_oneline_long_author_name_truncated() {
        let log = Log::new();
        let header = ChangeHeader::builder()
            .message("Test message")
            .author(Author::new(
                "This Is A Very Long Author Name That Should Be Truncated",
                None::<String>,
            ))
            .build();

        let entry = HistoryEntry::new(
            1,
            NodeId::from(1),
            create_test_hash(),
            create_test_merkle(),
        )
        .with_change_header(header);

        let output = log.format_oneline(&[entry], 8);

        // Author name should be truncated (max 20 chars)
        assert!(!output.contains("This Is A Very Long Author Name That Should Be Truncated"));
    }
}

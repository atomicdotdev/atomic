use super::*;

// Log Command

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

    /// Builder: set the count limit.
    pub fn with_count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Builder: set the stack name.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Builder: set tags-only filter.
    pub fn with_tags_only(mut self, tags_only: bool) -> Self {
        self.tags_only = tags_only;
        self
    }

    /// Builder: set path filter.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Builder: set the output format.
    pub fn with_format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Builder: set reverse order.
    pub fn with_reverse(mut self, reverse: bool) -> Self {
        self.reverse = reverse;
        self
    }

    /// Builder: set the starting sequence number.
    pub fn with_from(mut self, from: u64) -> Self {
        self.from = Some(from);
        self
    }

    /// Builder: set full hash display.
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
    pub(crate) fn build_history_options(&self) -> HistoryOptions {
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
    pub(crate) fn get_hash_length(&self) -> usize {
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
    pub(crate) fn format_default(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
        let mut output = String::new();

        for (i, entry) in entries.iter().enumerate() {
            // Add separator between entries
            if i > 0 {
                output.push('\n');
            }

            // Change header line with sequence number for scripting compatibility
            let hash_str = format_hash_with_length(&entry.hash, hash_length);
            let tagged_marker = if entry.is_tagged { " (tag)" } else { "" };
            let seq_str = format!("#{}", entry.sequence);
            output.push_str(&format!(
                "{} {} {}{}\n",
                hint(&seq_str),
                style_hash(&hash_str),
                style_hash("change"),
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
                    output.push_str(&format!("Author: {}\n", style_author(&author_line)));
                }
            }

            // Timestamp
            if let Some(ts) = entry.timestamp() {
                let formatted_time = format_timestamp(&ts);
                output.push_str(&format!("Date:   {}\n", style_timestamp(&formatted_time)));
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
    pub(crate) fn format_short(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
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
    pub(crate) fn format_oneline(&self, entries: &[HistoryEntry], hash_length: usize) -> String {
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
    pub(crate) fn format_json(&self, entries: &[HistoryEntry]) -> String {
        let json_entries: Vec<JsonLogEntry> = entries
            .iter()
            .map(|e| JsonLogEntry::from_entry(e))
            .collect();

        serde_json::to_string_pretty(&json_entries)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e))
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
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
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

// Helper Functions

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
pub fn truncate_string(s: &str, max_len: usize) -> String {
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

/// Format an author for display from an `Author` struct.
///
/// Combines name and optional email into a standard "Name <email>" format.
///
/// # Arguments
///
/// * `author` - The author to format
///
/// # Returns
///
/// Formatted author string.
pub fn format_author(author: &Author) -> String {
    if let Some(ref email) = author.email {
        format!("{} <{}>", author.name, email)
    } else {
        author.name.clone()
    }
}

/// Output configuration for log display.
///
/// Controls how log entries are formatted and filtered.
#[derive(Debug, Clone)]
pub struct LogOutputConfig {
    /// Output format.
    pub format: LogFormat,
    /// Whether to show full hashes.
    pub full_hash: bool,
    /// Hash display length.
    pub hash_length: usize,
    /// Maximum number of entries.
    pub count: Option<usize>,
    /// Whether to reverse order.
    pub reverse: bool,
    /// Whether to show only tagged entries.
    pub tags_only: bool,
    /// Path filter.
    pub path: Option<String>,
    /// Stack filter.
    pub stack: Option<String>,
    /// Starting sequence number.
    pub from_sequence: u64,
}

impl Default for LogOutputConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl LogOutputConfig {
    /// Create a new config with defaults.
    pub fn new() -> Self {
        Self {
            format: LogFormat::Default,
            full_hash: false,
            hash_length: DEFAULT_HASH_LENGTH,
            count: None,
            reverse: false,
            tags_only: false,
            path: None,
            stack: None,
            from_sequence: 0,
        }
    }

    /// Builder: set format.
    pub fn format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Builder: set full hash display.
    pub fn full_hash(mut self, full_hash: bool) -> Self {
        self.full_hash = full_hash;
        if full_hash {
            self.hash_length = 52;
        }
        self
    }

    /// Builder: set the hash display length.
    pub fn hash_length(mut self, len: usize) -> Self {
        self.hash_length = len;
        self
    }

    /// Builder: set count limit.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// Builder: set reverse order.
    pub fn reverse(mut self, reverse: bool) -> Self {
        self.reverse = reverse;
        self
    }

    /// Builder: set tags-only filter.
    pub fn tags_only(mut self, tags_only: bool) -> Self {
        self.tags_only = tags_only;
        self
    }

    /// Builder: set path filter.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Builder: set stack filter.
    pub fn stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Builder: set starting sequence number.
    pub fn from_sequence(mut self, seq: u64) -> Self {
        self.from_sequence = seq;
        self
    }
}

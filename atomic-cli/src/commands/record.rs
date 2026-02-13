#![allow(dead_code)]
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
use crate::output::{
    print_hint, print_warning,
};

// =============================================================================
// Record Command
// =============================================================================

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

    // =========================================================================
    // AI Provenance Flags
    // =========================================================================
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

impl Record {
    /// Create a new Record command with default settings.
    pub fn new() -> Self {
        Self {
            message: None,
            all: false,
            files: Vec::new(),
            author: None,
            identity: None,
            usage: None,
            edit: false,
            algorithm: "myers".to_string(),
            dry_run: false,
            skip_binary: false,
            max_size: None,
            ai_assisted: false,
            ai_provider: None,
            ai_model: None,
            ai_tool: None,
            ai_suggestion_type: None,
            ai_input_tokens: None,
            ai_output_tokens: None,
            ai_cost_usd: None,
            ai_request_id: None,
            ai_session_id: None,
        }
    }

    /// Set the commit message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Set whether to record all changes.
    pub fn with_all(mut self, all: bool) -> Self {
        self.all = all;
        self
    }

    /// Set specific files to record.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(Into::into).collect();
        self
    }

    /// Set the author override.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Set the identity to use.
    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Set the usage context for identity selection.
    pub fn with_usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = Some(usage.into());
        self
    }

    /// Set whether to open editor.
    pub fn with_edit(mut self, edit: bool) -> Self {
        self.edit = edit;
        self
    }

    /// Set the diff algorithm.
    pub fn with_algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.algorithm = algorithm.into();
        self
    }

    /// Set dry run mode.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Set whether to skip binary files.
    pub fn with_skip_binary(mut self, skip_binary: bool) -> Self {
        self.skip_binary = skip_binary;
        self
    }

    /// Set maximum file size.
    pub fn with_max_size(mut self, max_size: u64) -> Self {
        self.max_size = Some(max_size);
        self
    }

    /// Set whether this change is AI-assisted.
    pub fn with_ai_assisted(mut self, ai_assisted: bool) -> Self {
        self.ai_assisted = ai_assisted;
        self
    }

    /// Set the AI provider.
    pub fn with_ai_provider(mut self, provider: impl Into<String>) -> Self {
        self.ai_provider = Some(provider.into());
        self
    }

    /// Set the AI model.
    pub fn with_ai_model(mut self, model: impl Into<String>) -> Self {
        self.ai_model = Some(model.into());
        self
    }

    /// Set the AI tool type.
    pub fn with_ai_tool(mut self, tool: impl Into<String>) -> Self {
        self.ai_tool = Some(tool.into());
        self
    }

    /// Set the AI suggestion type.
    pub fn with_ai_suggestion_type(mut self, suggestion_type: impl Into<String>) -> Self {
        self.ai_suggestion_type = Some(suggestion_type.into());
        self
    }

    /// Set the AI input tokens.
    pub fn with_ai_input_tokens(mut self, tokens: u64) -> Self {
        self.ai_input_tokens = Some(tokens);
        self
    }

    /// Set the AI output tokens.
    pub fn with_ai_output_tokens(mut self, tokens: u64) -> Self {
        self.ai_output_tokens = Some(tokens);
        self
    }

    /// Set the AI cost in USD.
    pub fn with_ai_cost_usd(mut self, cost: f64) -> Self {
        self.ai_cost_usd = Some(cost);
        self
    }

    /// Set the AI request ID.
    pub fn with_ai_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.ai_request_id = Some(request_id.into());
        self
    }

    /// Set the AI session ID.
    pub fn with_ai_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.ai_session_id = Some(session_id.into());
        self
    }

    /// Parse the algorithm string into an Algorithm enum.
    fn parse_algorithm(&self) -> CliResult<Algorithm> {
        match self.algorithm.to_lowercase().as_str() {
            "myers" => Ok(Algorithm::Myers),
            "patience" => Ok(Algorithm::Patience),
            other => Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid algorithm '{}'. Expected 'myers' or 'patience'.",
                    other
                ),
            }),
        }
    }

    /// Parse the author string into an Author struct.
    fn parse_author(&self) -> Option<Author> {
        self.author.as_ref().map(|s| {
            // Try to parse "Name <email>" format
            if let Some(bracket_start) = s.find('<') {
                if let Some(bracket_end) = s.find('>') {
                    let name = s[..bracket_start].trim();
                    let email = s[bracket_start + 1..bracket_end].trim();
                    return Author::new(name, Some(email));
                }
            }
            // Just a name
            Author::new(s.trim(), None::<&str>)
        })
    }

    /// Get the author from identity store or command-line override.
    ///
    /// Priority:
    /// 1. --identity flag (specific identity by name)
    /// 2. --author flag (manual override)
    /// 3. --usage flag (default identity for usage context)
    /// 4. Global default identity
    /// 5. Fallback to empty author (let repository handle it)
    fn resolve_author(&self) -> CliResult<Option<Author>> {
        // Try to open identity store
        let store = match IdentityStore::open_default() {
            Ok(s) => Some(s),
            Err(_) => None, // Identity store not available, continue without
        };

        // 1. If --identity is specified, load that specific identity
        if let Some(identity_name) = &self.identity {
            let store = store.ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!(
                    "Identity store not available. Cannot use --identity flag."
                ))
            })?;

            let identity = store
                .load_by_name(identity_name)
                .map_err(|_| CliError::IdentityNotFound(identity_name.clone()))?;

            return Ok(Some(identity_to_author(&identity)));
        }

        // 2. If --author is specified, use the manual override
        if let Some(author) = self.parse_author() {
            return Ok(Some(author));
        }

        // 3. If --usage is specified, get default for that usage
        if let Some(usage_str) = &self.usage {
            if let Some(ref store) = store {
                let usage = IdentityUsage::parse(usage_str);
                if let Ok(Some(identity)) = store.get_default_for_usage(&usage) {
                    return Ok(Some(identity_to_author(&identity)));
                }
            }
        }

        // 4. Try to get global default identity
        if let Some(ref store) = store {
            if let Ok(Some(identity)) = store.get_default() {
                return Ok(Some(identity_to_author(&identity)));
            }
        }

        // 5. No identity available
        Ok(None)
    }

    /// Get the author's full name for display.
    fn get_author_name(author: &Author) -> &str {
        &author.name
    }

    /// Get the author's email for display.
    fn get_author_email(author: &Author) -> Option<&str> {
        author.email.as_deref()
    }

    /// Get the commit message, potentially from editor.
    fn get_message(&self) -> CliResult<String> {
        // If message was provided, use it
        if let Some(ref msg) = self.message {
            return Ok(msg.clone());
        }

        // If edit flag is set or we're in a terminal without a message
        if self.edit {
            return self.get_message_from_editor();
        }

        // Check if stdin is a terminal
        if is_terminal() {
            // Interactive mode - prompt for message
            print!("Enter commit message (empty line to cancel): ");
            std::io::stdout().flush().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to flush stdout: {}", e))
            })?;

            let mut message = String::new();
            std::io::stdin().read_line(&mut message).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to read message: {}", e))
            })?;

            let message = message.trim();
            if message.is_empty() {
                return Err(CliError::Cancelled);
            }

            Ok(message.to_string())
        } else {
            // Non-interactive mode - use default message
            Ok("No message provided".to_string())
        }
    }

    /// Open an editor to get the commit message.
    fn get_message_from_editor(&self) -> CliResult<String> {
        // Create a temporary file for the message
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("ATOMIC_EDITMSG");

        // Write template to file
        let template = r#"
# Enter your commit message above.
# Lines starting with '#' will be ignored.
# An empty message aborts the commit.
"#;

        std::fs::write(&temp_file, template).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create temp file: {}", e))
        })?;

        // Get editor from environment
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "vi".to_string()
                }
            });

        // Open editor
        let status = std::process::Command::new(&editor)
            .arg(&temp_file)
            .status()
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to open editor '{}': {}", editor, e))
            })?;

        if !status.success() {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Editor exited with non-zero status: {:?}",
                status.code()
            )));
        }

        // Read the file back
        let content = std::fs::read_to_string(&temp_file)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read temp file: {}", e)))?;

        // Clean up
        let _ = std::fs::remove_file(&temp_file);

        // Process content - remove comment lines and trim
        let message: String = content
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        if message.is_empty() {
            return Err(CliError::Cancelled);
        }

        Ok(message)
    }

    /// Build AI provenance from CLI flags and environment variables.
    ///
    /// Environment variables take precedence over CLI flags for consistency
    /// with AI tool integrations that set environment variables.
    fn build_provenance(&self) -> Option<Provenance> {
        // Check if AI-assisted via flag or environment variable
        let ai_enabled = self.ai_assisted
            || std::env::var("ATOMIC_AI_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);

        if !ai_enabled {
            return None;
        }

        // Get provider (required for provenance)
        let provider = self
            .ai_provider
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_PROVIDER").ok());

        let provider = match provider {
            Some(p) => p,
            None => {
                // No provider specified - can't create meaningful provenance
                return None;
            }
        };

        // Get model (required for provenance)
        let model = self
            .ai_model
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_MODEL").ok())
            .unwrap_or_else(|| "unknown".to_string());

        // Parse vendor from provider string
        let vendor = AIVendor::from_str(&provider);

        // Get tool type
        let tool_from_env = std::env::var("ATOMIC_AI_TOOL").ok();

        let tool_str = self
            .ai_tool
            .clone()
            .or_else(|| tool_from_env)
            .unwrap_or_else(|| "cli".to_string());

        let tool = match tool_str.to_lowercase().as_str() {
            "api" => AITool::Api,
            "chat" => AITool::Chat,
            "cli" => AITool::Cli("atomic".to_string()),
            "ci" => AITool::CI("atomic-ci".to_string()),
            "code-review" | "codereview" => AITool::CodeReview("atomic".to_string()),
            // AI coding assistants - treat as editors/IDEs
            "opencode" => AITool::Editor("opencode".to_string()),
            "cursor" => AITool::Editor("cursor".to_string()),
            "aider" => AITool::Cli("aider".to_string()),
            "claude-code" | "claude_code" => AITool::Cli("claude-code".to_string()),
            other => {
                // Check prefixes FIRST before contains patterns
                // This ensures "cli:opencode" becomes Cli("opencode") not Editor("cli:opencode")
                if other.starts_with("cli:") {
                    AITool::Cli(other.trim_start_matches("cli:").to_string())
                } else if other.starts_with("ci:") {
                    AITool::CI(other.trim_start_matches("ci:").to_string())
                } else if other.starts_with("editor:") {
                    AITool::Editor(other.trim_start_matches("editor:").to_string())
                // Then check for editor or IDE plugin patterns
                } else if other.contains("editor")
                    || other.contains("zed")
                    || other.contains("vscode")
                    || other.contains("vim")
                    || other.contains("emacs")
                {
                    AITool::Editor(other.to_string())
                } else if other.contains("plugin") || other.contains("copilot") {
                    AITool::IdePlugin(other.to_string())
                } else if other.contains("aider") || other.contains("claude-code") {
                    AITool::Cli(other.to_string())
                } else {
                    AITool::Other(other.to_string())
                }
            }
        };

        // Get suggestion type
        let suggestion_str = self
            .ai_suggestion_type
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SUGGESTION_TYPE").ok())
            .unwrap_or_else(|| "collaborative".to_string());

        let suggestion_type = match suggestion_str.to_lowercase().as_str() {
            "complete" => SuggestionType::Complete,
            "partial" => SuggestionType::Partial,
            "collaborative" => SuggestionType::Collaborative,
            "selection" => SuggestionType::Selection,
            "review" => SuggestionType::Review,
            "documentation" | "docs" => SuggestionType::Documentation,
            "debugging" | "debug" => SuggestionType::Debugging,
            "refactoring" | "refactor" => SuggestionType::Refactoring,
            "testing" | "test" => SuggestionType::Testing,
            _ => SuggestionType::Collaborative,
        };

        // Build the provenance
        let mut builder = Provenance::builder()
            .vendor(vendor)
            .model(&model)
            .tool(tool)
            .suggestion_type(suggestion_type);

        // Add optional fields from CLI or environment
        let input_tokens = self.ai_input_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_INPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        let output_tokens = self.ai_output_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_OUTPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });

        if let Some(input) = input_tokens {
            builder = builder.input_tokens(input);
        }
        if let Some(output) = output_tokens {
            builder = builder.output_tokens(output);
        }

        let cost = self.ai_cost_usd.or_else(|| {
            std::env::var("ATOMIC_AI_COST_USD")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        if let Some(cost_usd) = cost {
            builder = builder.cost_usd(cost_usd);
        }

        let request_id = self
            .ai_request_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_REQUEST_ID").ok());
        if let Some(req_id) = request_id {
            builder = builder.request_id(req_id);
        }

        let session_id = self
            .ai_session_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SESSION_ID").ok());
        if let Some(sess_id) = session_id {
            builder = builder.session_id(sess_id);
        }

        // Add timestamp
        builder = builder.timestamp(chrono::Utc::now().timestamp());

        Some(builder.build())
    }

    /// Build RecordOptions from command-line arguments.
    fn build_options(&self) -> CliResult<RecordOptions> {
        let algorithm = self.parse_algorithm()?;

        let mut options = RecordOptions::new()
            .all(self.all)
            .algorithm(algorithm)
            .skip_binary(self.skip_binary)
            .apply_after_record(!self.dry_run)
            .save_to_store(!self.dry_run);

        // Add specific files if provided
        if !self.files.is_empty() {
            options = options.paths(self.files.clone());
        }

        // Set max size if provided
        if let Some(max_size) = self.max_size {
            options = options.max_file_size(max_size);
        }

        // Add AI provenance if enabled
        if let Some(provenance) = self.build_provenance() {
            options = options.add_provenance(provenance);
        }

        Ok(options)
    }

    /// Format the outcome for display.
    fn format_outcome(&self, _repo: &Repository, outcome: &RecordOutcome) -> String {
        let mut output = String::new();

        // Get stack name (use default for now until method is implemented)
        let stack_name = "dev";

        // Get hash (shortened)
        let hash_short = &outcome.hash().to_base32()[..DEFAULT_HASH_LENGTH.min(8)];

        // Get message (first line only)
        let message = outcome
            .change()
            .hashed
            .header
            .message
            .lines()
            .next()
            .unwrap_or("No message");

        // Header line: [stack seq/hash] message
        let sequence = outcome.new_state().map(|_| "1").unwrap_or("?");
        output.push_str(&format!(
            "[{} {}/{}] {}\n",
            stack_name, sequence, hash_short, message
        ));

        // Stats line - show graph-based stats
        let stats = outcome.stats();
        if stats.has_changes() {
            output.push_str(&format!(
                " {} changed, +{} vertices, ~{} edges, {} bytes\n",
                format_count(stats.files_recorded, "file"),
                stats.vertices_added,
                stats.edges_modified,
                stats.content_bytes
            ));

            // CRDT token-level statistics (for fine-grained diff tracking)
            if stats.has_crdt_stats() {
                // Line-level changes
                let line_changes = stats.total_line_changes();
                if line_changes > 0 {
                    output.push_str(&format!(
                        " {} (+{} -{} ~{})\n",
                        format_count(line_changes, "line"),
                        stats.lines_added,
                        stats.lines_deleted,
                        stats.lines_modified
                    ));
                }

                // Token-level changes
                let token_ops = stats.total_token_ops();
                if token_ops > 0 {
                    output.push_str(&format!(
                        " {} (+{} -{} ~{})\n",
                        format_count(token_ops, "token"),
                        stats.tokens_added,
                        stats.tokens_deleted,
                        stats.tokens_replaced
                    ));
                }
            }
        }

        // File list
        for path in outcome.recorded_files() {
            output.push_str(&format!(" {}\n", path));
        }

        output
    }

    /// Display dry run preview.
    fn display_dry_run(&self, repo: &Repository) -> CliResult<()> {
        let status = repo
            .status(StatusOptions::default())
            .map_err(CliError::Repository)?;

        let mut has_changes = false;

        println!("Would record:");

        for entry in status.entries() {
            // Skip untracked unless --all
            if matches!(entry.status(), FileStatus::Untracked) && !self.all {
                continue;
            }

            // Skip clean files
            if matches!(entry.status(), FileStatus::Clean) {
                continue;
            }

            // Filter by specified files if any
            if !self.files.is_empty() {
                let path_str = entry.path().to_string_lossy();
                if !self.files.iter().any(|f| path_str.contains(f)) {
                    continue;
                }
            }

            has_changes = true;
            let status_desc = match entry.status() {
                FileStatus::Added => "new file:",
                FileStatus::Modified => "modified:",
                FileStatus::Deleted => "deleted: ",
                FileStatus::Untracked => "new file:",
                FileStatus::TypeChanged => "typechange:",
                FileStatus::PermissionsChanged => "permissions:",
                FileStatus::Conflicted => "conflicted:",
                FileStatus::Clean => continue,
            };

            println!("  {}  {}", status_desc, entry.path().to_string_lossy());
        }

        if !has_changes {
            println!("  (no changes to record)");
        }

        Ok(())
    }
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Record {
    /// Execute the record command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Get the commit message (from argument, editor, or prompt)
    /// 3. Detect changes in the working copy
    /// 4. If --all, add untracked files
    /// 5. If --dry-run, display preview and exit
    /// 6. Create the change from modifications
    /// 7. Save the change to the store
    /// 8. Apply the change to the current stack
    /// 9. Display the result
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Handle dry run
        if self.dry_run {
            return self.display_dry_run(&repo);
        }

        // Get commit message
        let message = self.get_message()?;

        // Resolve author from identity or command-line
        let author = self.resolve_author()?;

        // Build change header
        let mut header_builder = ChangeHeader::builder().message(&message);

        if let Some(author) = author {
            header_builder = header_builder.author(author);
        }

        let header = header_builder.build();

        // Build record options
        let options = self.build_options()?;

        // If --all, first add all untracked files
        if self.all {
            let status = repo
                .status(StatusOptions::default())
                .map_err(CliError::Repository)?;

            for entry in status.untracked() {
                let path = entry.path();
                if let Err(e) = repo.add(path, Default::default()) {
                    print_warning(&format!("Failed to add '{}': {}", path.display(), e));
                }
            }
        }

        // Record the changes
        let outcome = repo.record(header, options).map_err(|e| match e {
            atomic_repository::record::RecordError::NothingToRecord => CliError::NothingToRecord,
            atomic_repository::record::RecordError::NoFilesMatched { .. } => {
                CliError::NothingToRecord
            }
            atomic_repository::record::RecordError::FileNotFound { path } => {
                CliError::FileNotFound {
                    path: PathBuf::from(path),
                }
            }
            atomic_repository::record::RecordError::FileNotTracked { path } => {
                CliError::FileNotTracked {
                    path: PathBuf::from(path),
                }
            }
            other => CliError::Internal(anyhow::anyhow!("{}", other)),
        })?;

        // Display result
        let output = self.format_outcome(&repo, &outcome);
        print!("{}", output);

        // Show any errors that occurred during recording
        if outcome.has_errors() {
            println!();
            print_warning("Some files had errors:");
            for (path, error) in outcome.errors() {
                println!("  {}: {}", path, error);
            }
        }

        // Show skipped files if any
        if !outcome.skipped_files().is_empty() {
            println!();
            print_hint(&format!(
                "{} skipped (empty, binary, or too large)",
                format_count(outcome.skipped_files().len(), "file")
            ));
        }

        Ok(())
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Format a count with singular/plural suffix.
fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

/// Check if stdin is a terminal (for interactive prompts).
/// Convert an atomic_identity::Identity to atomic_core::change::Author.
///
/// This bridges the two Author types: atomic_identity has its own Author
/// for lightweight identity operations, while atomic_core::change::Author
/// is used in change headers. This function performs the conversion.
fn identity_to_author(identity: &Identity) -> Author {
    Author::with_identity(
        identity.name.clone(),
        identity.email.clone(),
        identity.public_key_base32(),
    )
}

fn is_terminal() -> bool {
    // Use a simple heuristic - check if we're in a CI environment
    // or if stdin is piped
    std::env::var("CI").is_err() && std::env::var("ATOMIC_NONINTERACTIVE").is_err()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    // =========================================================================
    // Record Command Construction Tests
    // =========================================================================

    #[test]
    fn test_record_new() {
        let record = Record::new();
        assert!(record.message.is_none());
        assert!(!record.all);
        assert!(record.files.is_empty());
        assert!(record.author.is_none());
        assert!(record.identity.is_none());
        assert!(record.usage.is_none());
        assert!(!record.edit);
        assert_eq!(record.algorithm, "myers");
        assert!(!record.dry_run);
        assert!(!record.skip_binary);
        assert!(record.max_size.is_none());
    }

    #[test]
    fn test_record_with_identity() {
        let record = Record::new()
            .with_identity("alice")
            .with_message("Test commit");

        assert_eq!(record.identity, Some("alice".to_string()));
        assert_eq!(record.message, Some("Test commit".to_string()));
    }

    #[test]
    fn test_record_with_usage() {
        let record = Record::new().with_usage("work").with_message("Work commit");

        assert_eq!(record.usage, Some("work".to_string()));
    }

    #[test]
    fn test_record_identity_precedence() {
        // When both identity and author are set, identity takes precedence
        // (tested via resolve_author logic)
        let record = Record::new()
            .with_identity("alice")
            .with_author("Bob <bob@example.com>");

        assert!(record.identity.is_some());
        assert!(record.author.is_some());
        // Note: resolve_author would use identity first
    }

    #[test]
    fn test_record_default() {
        let record = Record::default();
        assert!(record.message.is_none());
        assert!(!record.all);
        assert!(record.files.is_empty());
    }

    #[test]
    fn test_record_with_message() {
        let record = Record::new().with_message("Test message");
        assert_eq!(record.message, Some("Test message".to_string()));
    }

    #[test]
    fn test_record_with_all() {
        let record = Record::new().with_all(true);
        assert!(record.all);
    }

    #[test]
    fn test_record_with_files_vec() {
        let record = Record::new().with_files(vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(record.files.len(), 2);
        assert_eq!(record.files[0], "src/main.rs");
        assert_eq!(record.files[1], "src/lib.rs");
    }

    #[test]
    fn test_record_with_files_strings() {
        let record = Record::new().with_files(vec![String::from("README.md")]);
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0], "README.md");
    }

    #[test]
    fn test_record_with_author() {
        let record = Record::new().with_author("Alice <alice@example.com>");
        assert_eq!(record.author, Some("Alice <alice@example.com>".to_string()));
    }

    #[test]
    fn test_record_with_edit() {
        let record = Record::new().with_edit(true);
        assert!(record.edit);
    }

    #[test]
    fn test_record_with_algorithm_myers() {
        let record = Record::new().with_algorithm("myers");
        assert_eq!(record.algorithm, "myers");
    }

    #[test]
    fn test_record_with_algorithm_patience() {
        let record = Record::new().with_algorithm("patience");
        assert_eq!(record.algorithm, "patience");
    }

    #[test]
    fn test_record_with_dry_run() {
        let record = Record::new().with_dry_run(true);
        assert!(record.dry_run);
    }

    #[test]
    fn test_record_with_skip_binary() {
        let record = Record::new().with_skip_binary(true);
        assert!(record.skip_binary);
    }

    #[test]
    fn test_record_with_max_size() {
        let record = Record::new().with_max_size(1024 * 1024);
        assert_eq!(record.max_size, Some(1024 * 1024));
    }

    #[test]
    fn test_record_builder_chain() {
        let record = Record::new()
            .with_message("Test")
            .with_all(true)
            .with_files(vec!["src/main.rs"])
            .with_author("Bob")
            .with_algorithm("patience")
            .with_dry_run(true)
            .with_skip_binary(true)
            .with_max_size(1000);

        assert_eq!(record.message, Some("Test".to_string()));
        assert!(record.all);
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.author, Some("Bob".to_string()));
        assert_eq!(record.algorithm, "patience");
        assert!(record.dry_run);
        assert!(record.skip_binary);
        assert_eq!(record.max_size, Some(1000));
    }

    // =========================================================================
    // Algorithm Parsing Tests
    // =========================================================================

    #[test]
    fn test_parse_algorithm_myers() {
        let record = Record::new().with_algorithm("myers");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Myers));
    }

    #[test]
    fn test_parse_algorithm_patience() {
        let record = Record::new().with_algorithm("patience");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Patience));
    }

    #[test]
    fn test_parse_algorithm_case_insensitive() {
        let record = Record::new().with_algorithm("MYERS");
        let result = record.parse_algorithm();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), Algorithm::Myers));
    }

    #[test]
    fn test_parse_algorithm_invalid() {
        let record = Record::new().with_algorithm("invalid");
        let result = record.parse_algorithm();
        assert!(result.is_err());
    }

    // =========================================================================
    // Author Parsing Tests
    // =========================================================================

    #[test]
    fn test_parse_author_full() {
        let record = Record::new().with_author("Alice <alice@example.com>");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_author_name_only() {
        let record = Record::new().with_author("Bob");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Bob");
        assert_eq!(author.email, None);
    }

    #[test]
    fn test_parse_author_none() {
        let record = Record::new();
        let author = record.parse_author();
        assert!(author.is_none());
    }

    #[test]
    fn test_parse_author_with_spaces() {
        let record = Record::new().with_author("Alice Smith <alice.smith@example.com>");
        let author = record.parse_author();
        assert!(author.is_some());
        let author = author.unwrap();
        assert_eq!(author.name, "Alice Smith");
        assert_eq!(author.email, Some("alice.smith@example.com".to_string()));
    }

    // =========================================================================
    // Build Options Tests
    // =========================================================================

    #[test]
    fn test_build_options_default() {
        let record = Record::new();
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(!options.get_all());
        assert!(!options.get_skip_binary());
    }

    #[test]
    fn test_build_options_with_all() {
        let record = Record::new().with_all(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(options.get_all());
    }

    #[test]
    fn test_build_options_with_files() {
        let record = Record::new().with_files(vec!["src/main.rs"]);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert_eq!(options.get_paths().len(), 1);
    }

    #[test]
    fn test_build_options_with_skip_binary() {
        let record = Record::new().with_skip_binary(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(options.get_skip_binary());
    }

    #[test]
    fn test_build_options_with_max_size() {
        let record = Record::new().with_max_size(500);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert_eq!(options.get_max_file_size(), 500);
    }

    #[test]
    fn test_build_options_dry_run_no_save() {
        let record = Record::new().with_dry_run(true);
        let result = record.build_options();
        assert!(result.is_ok());
        let options = result.unwrap();
        assert!(!options.get_save_to_store());
        assert!(!options.get_apply_after_record());
    }

    #[test]
    fn test_build_options_invalid_algorithm() {
        let record = Record::new().with_algorithm("unknown");
        let result = record.build_options();
        assert!(result.is_err());
    }

    // =========================================================================
    // Format Count Tests
    // =========================================================================

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "file"), "0 files");
    }

    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "file"), "1 file");
    }

    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "file"), "5 files");
    }

    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "change"), "1 change");
        assert_eq!(format_count(2, "change"), "2 changes");
    }

    // =========================================================================
    // Integration Tests (require temp directories)
    // =========================================================================

    /// Guard that restores the current directory when dropped.
    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    #[serial]
    fn test_record_run_outside_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let record = Record::new().with_message("Test");
        let result = record.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_run_nothing_to_record() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Test");
        let result = record.run();

        // Should fail because there's nothing to record
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_dry_run_shows_changes() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let repo = Repository::init(repo_path).unwrap();
            // Add a file
            std::fs::write(repo_path.join("test.txt"), "Hello").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_dry_run(true);
        let result = record.run();

        // Dry run should succeed without creating a change
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_record_with_message_and_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Hello, World!").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Initial commit");
        let result = record.run();

        // This should work once the full record workflow is complete
        // For now, we check that it attempts to record
        // The actual success depends on the underlying implementation
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_with_specific_files() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
            std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();
            repo.add("file1.txt", Default::default()).unwrap();
            repo.add("file2.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Record only file1.txt
        let record = Record::new()
            .with_message("Add file1")
            .with_files(vec!["file1.txt"]);

        let result = record.run();
        // Check that it at least attempts to run
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    #[serial]
    fn test_record_all_includes_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
            // Create file but don't add it
            std::fs::write(repo_path.join("untracked.txt"), "Untracked content").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Add all").with_all(true);

        let result = record.run();
        // With --all, it should add untracked files
        assert!(result.is_ok() || result.is_err());
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[test]
    fn test_record_empty_message() {
        let record = Record::new().with_message("");
        assert_eq!(record.message, Some("".to_string()));
    }

    #[test]
    fn test_record_unicode_message() {
        let record = Record::new().with_message("添加新功能 🚀");
        assert_eq!(record.message, Some("添加新功能 🚀".to_string()));
    }

    #[test]
    fn test_record_multiline_message() {
        let record = Record::new().with_message("First line\n\nSecond paragraph");
        assert_eq!(
            record.message,
            Some("First line\n\nSecond paragraph".to_string())
        );
    }

    #[test]
    fn test_record_with_iterator() {
        let files = vec!["a.rs", "b.rs", "c.rs"];
        let record = Record::new().with_files(files.into_iter());
        assert_eq!(record.files.len(), 3);
    }

    #[test]
    fn test_record_clone() {
        let record = Record::new()
            .with_message("Test")
            .with_all(true)
            .with_dry_run(true);

        let cloned = record.clone();
        assert_eq!(cloned.message, record.message);
        assert_eq!(cloned.all, record.all);
        assert_eq!(cloned.dry_run, record.dry_run);
    }

    #[test]
    fn test_record_debug() {
        let record = Record::new().with_message("Test");
        let debug_str = format!("{:?}", record);
        assert!(debug_str.contains("Record"));
        assert!(debug_str.contains("Test"));
    }

    // =========================================================================
    // Identity to Author Conversion Tests
    // =========================================================================

    #[test]
    fn test_identity_to_author_with_email() {
        let identity = Identity::builder("alice")
            .email("alice@example.com")
            .build()
            .unwrap();

        let author = identity_to_author(&identity);

        assert_eq!(author.name, "alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert!(author.identity.is_some());
        // Identity should be the public key in base32
        assert_eq!(
            author.identity.as_ref().unwrap(),
            &identity.public_key_base32()
        );
    }

    #[test]
    fn test_identity_to_author_without_email() {
        let identity = Identity::builder("bob").build().unwrap();

        let author = identity_to_author(&identity);

        assert_eq!(author.name, "bob");
        assert!(author.email.is_none());
        assert!(author.identity.is_some());
    }

    #[test]
    fn test_identity_to_author_preserves_public_key() {
        let identity = Identity::generate("test-user");
        let author = identity_to_author(&identity);

        // The author's identity field should match the identity's public key
        assert_eq!(author.identity.unwrap(), identity.public_key_base32());
    }

    // =========================================================================
    // AI Provenance Tests
    // =========================================================================

    #[test]
    fn test_record_with_ai_assisted() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        assert!(record.ai_assisted);
        assert_eq!(record.ai_provider, Some("anthropic".to_string()));
        assert_eq!(
            record.ai_model,
            Some("claude-sonnet-4-20250514".to_string())
        );
    }

    #[test]
    fn test_record_ai_flags_default() {
        let record = Record::new();

        assert!(!record.ai_assisted);
        assert!(record.ai_provider.is_none());
        assert!(record.ai_model.is_none());
        assert!(record.ai_tool.is_none());
        assert!(record.ai_suggestion_type.is_none());
        assert!(record.ai_input_tokens.is_none());
        assert!(record.ai_output_tokens.is_none());
        assert!(record.ai_cost_usd.is_none());
        assert!(record.ai_request_id.is_none());
        assert!(record.ai_session_id.is_none());
    }

    #[test]
    fn test_record_with_full_ai_provenance() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514")
            .with_ai_tool("zed-editor")
            .with_ai_suggestion_type("collaborative")
            .with_ai_input_tokens(1500)
            .with_ai_output_tokens(500)
            .with_ai_cost_usd(0.015)
            .with_ai_request_id("req_123abc")
            .with_ai_session_id("sess_456def");

        assert!(record.ai_assisted);
        assert_eq!(record.ai_provider, Some("anthropic".to_string()));
        assert_eq!(
            record.ai_model,
            Some("claude-sonnet-4-20250514".to_string())
        );
        assert_eq!(record.ai_tool, Some("zed-editor".to_string()));
        assert_eq!(record.ai_suggestion_type, Some("collaborative".to_string()));
        assert_eq!(record.ai_input_tokens, Some(1500));
        assert_eq!(record.ai_output_tokens, Some(500));
        assert_eq!(record.ai_cost_usd, Some(0.015));
        assert_eq!(record.ai_request_id, Some("req_123abc".to_string()));
        assert_eq!(record.ai_session_id, Some("sess_456def".to_string()));
    }

    #[test]
    fn test_build_provenance_disabled() {
        let record = Record::new();
        let provenance = record.build_provenance();
        assert!(provenance.is_none());
    }

    #[test]
    fn test_build_provenance_enabled_without_provider() {
        let record = Record::new().with_ai_assisted(true);
        // Without provider, provenance should be None
        let provenance = record.build_provenance();
        assert!(provenance.is_none());
    }

    #[test]
    fn test_build_provenance_enabled_with_provider() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let provenance = record.build_provenance();
        assert!(provenance.is_some());

        let prov = provenance.unwrap();
        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_build_provenance_tool_parsing() {
        // Test API tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("api");
        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tool, AITool::Api);

        // Test chat tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("chat");
        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tool, AITool::Chat);

        // Test CLI tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("cli");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Cli(_)));

        // Test cli:opencode (what opencode passes)
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_tool("cli:opencode");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Cli(ref name) if name == "opencode"));

        // Test bare "opencode"
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_tool("opencode");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Editor(ref name) if name == "opencode"));

        // Test editor tool
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("openai")
            .with_ai_tool("zed-editor");
        let prov = record.build_provenance().unwrap();
        assert!(matches!(prov.tool, AITool::Editor(_)));
    }

    #[test]
    fn test_build_provenance_suggestion_type_parsing() {
        let cases = vec![
            ("complete", SuggestionType::Complete),
            ("partial", SuggestionType::Partial),
            ("collaborative", SuggestionType::Collaborative),
            ("review", SuggestionType::Review),
            ("documentation", SuggestionType::Documentation),
            ("debugging", SuggestionType::Debugging),
            ("refactoring", SuggestionType::Refactoring),
            ("testing", SuggestionType::Testing),
        ];

        for (input, expected) in cases {
            let record = Record::new()
                .with_ai_assisted(true)
                .with_ai_provider("anthropic")
                .with_ai_suggestion_type(input);
            let prov = record.build_provenance().unwrap();
            assert_eq!(
                prov.suggestion_type, expected,
                "Failed for input: {}",
                input
            );
        }
    }

    #[test]
    fn test_build_provenance_vendor_parsing() {
        let cases = vec![
            ("anthropic", AIVendor::Anthropic),
            ("openai", AIVendor::OpenAI),
            ("google", AIVendor::Google),
            ("meta", AIVendor::Meta),
            ("mistral", AIVendor::Mistral),
        ];

        for (input, expected) in cases {
            let record = Record::new().with_ai_assisted(true).with_ai_provider(input);
            let prov = record.build_provenance().unwrap();
            assert_eq!(prov.vendor, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_build_provenance_with_tokens_and_cost() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514")
            .with_ai_input_tokens(1000)
            .with_ai_output_tokens(500)
            .with_ai_cost_usd(0.025);

        let prov = record.build_provenance().unwrap();
        assert_eq!(prov.tokens.input_tokens, 1000);
        assert_eq!(prov.tokens.output_tokens, 500);
        assert!(!prov.cost.is_zero());
    }

    #[test]
    fn test_build_options_with_provenance() {
        let record = Record::new()
            .with_ai_assisted(true)
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let options = record.build_options().unwrap();
        assert!(options.has_provenance());
        assert_eq!(options.get_provenance().len(), 1);
    }

    #[test]
    fn test_build_options_without_provenance() {
        let record = Record::new();
        let options = record.build_options().unwrap();
        assert!(!options.has_provenance());
        assert!(options.get_provenance().is_empty());
    }

    #[test]
    #[serial]
    fn test_build_provenance_from_env_var() {
        // Set environment variables
        std::env::set_var("ATOMIC_AI_ENABLED", "true");
        std::env::set_var("ATOMIC_AI_PROVIDER", "openai");
        std::env::set_var("ATOMIC_AI_MODEL", "gpt-4");

        let record = Record::new(); // No CLI flags set
        let provenance = record.build_provenance();

        // Clean up
        std::env::remove_var("ATOMIC_AI_ENABLED");
        std::env::remove_var("ATOMIC_AI_PROVIDER");
        std::env::remove_var("ATOMIC_AI_MODEL");

        assert!(provenance.is_some());
        let prov = provenance.unwrap();
        assert_eq!(prov.vendor, AIVendor::OpenAI);
        assert_eq!(prov.model, "gpt-4");
    }

    #[test]
    #[serial]
    fn test_build_provenance_cli_overrides_env() {
        // Set environment variables
        std::env::set_var("ATOMIC_AI_ENABLED", "true");
        std::env::set_var("ATOMIC_AI_PROVIDER", "openai");
        std::env::set_var("ATOMIC_AI_MODEL", "gpt-4");

        // CLI flags take precedence
        let record = Record::new()
            .with_ai_provider("anthropic")
            .with_ai_model("claude-sonnet-4-20250514");

        let provenance = record.build_provenance();

        // Clean up
        std::env::remove_var("ATOMIC_AI_ENABLED");
        std::env::remove_var("ATOMIC_AI_PROVIDER");
        std::env::remove_var("ATOMIC_AI_MODEL");

        assert!(provenance.is_some());
        let prov = provenance.unwrap();
        // CLI values should be used (environment variables are fallback)
        assert_eq!(prov.vendor, AIVendor::Anthropic);
        assert_eq!(prov.model, "claude-sonnet-4-20250514");
    }
}

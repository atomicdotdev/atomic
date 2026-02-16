use super::*;

// Change Command

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

// Helper Functions

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

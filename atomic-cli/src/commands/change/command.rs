use clap_complete::engine::ArgValueCompleter;

use crate::commands::complete::{complete_change_hashes, complete_view_names};

use super::*;

// Change Command

/// Show details for a specific change.
///
/// The `change` command displays detailed information about a change (patch)
/// in the repository. Changes can be identified by:
///
/// - **Full hash**: The complete 52-character Base32 hash
/// - **Hash prefix**: An unambiguous prefix (minimum 4 characters)
/// - **Sequence number**: Index in the view's history (`42` or `#42`)
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
    /// If omitted, shows the most recent change on the current view.
    /// Sequence numbers can be prefixed with `#` (e.g., `#42`).
    #[arg(value_name = "HASH_OR_SEQ", add = ArgValueCompleter::new(complete_change_hashes))]
    pub identifier: Option<String>,

    /// View to use for sequence lookup.
    ///
    /// When looking up by sequence number, use this view instead
    /// of the current view.
    #[arg(long = "view", value_name = "NAME", add = ArgValueCompleter::new(complete_view_names))]
    pub view: Option<String>,

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
}

impl ChangeCmd {
    /// Create a new ChangeCmd with default settings.
    pub fn new() -> Self {
        Self {
            identifier: None,
            view: None,
            format: ChangeFormat::Default,
            show_deps: false,
            show_hunks: false,
            full_hash: false,
        }
    }

    /// Builder: set the identifier.
    pub fn with_identifier(mut self, id: impl Into<String>) -> Self {
        self.identifier = Some(id.into());
        self
    }

    /// Builder: set the view.
    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    /// Builder: set the output format.
    pub fn with_format(mut self, format: ChangeFormat) -> Self {
        self.format = format;
        self
    }

    /// Builder: set show-deps flag.
    pub fn with_show_deps(mut self, show_deps: bool) -> Self {
        self.show_deps = show_deps;
        self
    }

    /// Builder: set show-hunks flag.
    pub fn with_show_hunks(mut self, show_hunks: bool) -> Self {
        self.show_hunks = show_hunks;
        self
    }

    /// Builder: set full-hash flag.
    pub fn with_full_hash(mut self, full_hash: bool) -> Self {
        self.full_hash = full_hash;
        self
    }

    /// Get the hash display length.
    pub(crate) fn get_hash_length(&self) -> usize {
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

        let view_name = self.view.as_deref().unwrap_or_else(|| repo.current_view());

        match id {
            ChangeIdentifier::FullHash(hash) => {
                // Verify the change exists
                if !repo.has_change(&hash) {
                    return Err(CliError::ChangeNotFound {
                        hash: hash.to_base32(),
                    });
                }

                // Try to find sequence number
                let seq = self.find_sequence_for_hash(repo, view_name, &hash)?;
                Ok((hash, seq))
            }

            ChangeIdentifier::HashPrefix(prefix) => {
                let (hash, seq) = self.resolve_hash_prefix(repo, view_name, &prefix)?;
                Ok((hash, seq))
            }

            ChangeIdentifier::Sequence(seq) => {
                let hash = self.resolve_sequence(repo, view_name, seq)?;
                Ok((hash, Some(seq)))
            }

            ChangeIdentifier::Latest => {
                let (hash, seq) = self.get_latest_change(repo, view_name)?;
                Ok((hash, Some(seq)))
            }
        }
    }

    /// Find the sequence number for a hash in the current view.
    fn find_sequence_for_hash(
        &self,
        repo: &Repository,
        view_name: &str,
        hash: &Hash,
    ) -> CliResult<Option<u64>> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        find_change_sequence(&txn, &view, hash)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))
    }

    /// Resolve a hash prefix to a full hash.
    fn resolve_hash_prefix(
        &self,
        repo: &Repository,
        view_name: &str,
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
                let seq = self.find_sequence_for_hash(repo, view_name, &hash)?;
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
    fn resolve_sequence(&self, repo: &Repository, view_name: &str, seq: u64) -> CliResult<Hash> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        let entry = get_change_at_sequence(&txn, &view, seq).map_err(|e| match e {
            atomic_repository::history::HistoryError::SequenceOutOfRange { sequence, max } => {
                CliError::InvalidArgument {
                    message: format!(
                        "Sequence {} out of range. View has {} changes (0-{}).",
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

    /// Get the most recent change on the view.
    fn get_latest_change(&self, repo: &Repository, view_name: &str) -> CliResult<(Hash, u64)> {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?
            .ok_or_else(|| CliError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        if view.change_count == 0 {
            return Err(CliError::NothingToRecord);
        }

        let seq = view.change_count - 1;
        let entry = get_change_at_sequence(&txn, &view, seq)
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

        // Git import metadata (if present)
        if let Some(ref unhashed) = change.unhashed {
            if let Some(git) = unhashed.get("git") {
                output.push('\n');
                output.push_str("Git Import:\n");
                if let Some(repo) = git.get("repository").and_then(|v| v.as_str()) {
                    output.push_str(&format!("  Repository: {}\n", repo));
                }
                if let Some(sha) = git.get("sha").and_then(|v| v.as_str()) {
                    output.push_str(&format!("  Commit: {}\n", &sha[..12.min(sha.len())]));
                }
                if git
                    .get("empty_commit")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    output.push_str(&format!("  {}\n", hint("(empty commit)")));
                }
                if git
                    .get("empty_merge")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    output.push_str(&format!("  {}\n", hint("(merge commit)")));
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

            // Show one summary row per path. Git imports and large records can
            // legitimately contain thousands of graph ops for a small number
            // of files; printing each op makes the change view unusable.
            for summary in hunk_display_summaries(&change.hashed.hunks) {
                output.push_str(&format!(
                    "  {} {} {}\n",
                    summary.symbol,
                    style_path(&summary.path),
                    hint(&summary.info)
                ));
            }
        }

        // Attestation (inline AI metadata from change header)
        if change.has_provenance() {
            output.push('\n');
            if let Some(prov) = change.hashed.provenance.first() {
                output.push_str(&self.format_attestation(prov));
            }
        }

        // Change ledger (causal decision DAG from .provenance file)
        output.push_str(&self.format_change_ledger(hash, repo));

        output
    }

    /// Format the change for short output.
    pub(crate) fn format_short(
        &self,
        change: &Change,
        hash: &Hash,
        sequence: Option<u64>,
    ) -> String {
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

    /// Format AI attestation information for display.
    fn format_attestation(&self, prov: &Provenance) -> String {
        let mut output = String::new();

        output.push_str(&format!("{}\n", emphasis("=== Attestation ===")));
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
                // turn_number is recorded as 1-indexed but session ledger
                // turns are 0-indexed; display the 0-indexed value.
                let display_value = if key == "turn_number" {
                    value
                        .parse::<u32>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| value.clone())
                } else {
                    value.clone()
                };
                output.push_str(&format!("    {}: {}\n", hint(key), info(&display_value)));
            }
        }

        output
    }

    /// Format the change ledger (causal decision DAG) for display.
    ///
    /// Displays the structured provenance graph stored in the `.provenance`
    /// file: goals, tool executions, explorations, commitments, and patch
    /// proposals that led to this change.
    fn format_change_ledger(&self, change_hash: &Hash, repo: &Repository) -> String {
        let mut output = String::new();

        let graphs = match repo.find_provenance_for_change(change_hash) {
            Ok(g) if !g.is_empty() => g,
            Ok(_) => {
                output.push_str(&format!(
                    "{}\n",
                    hint("No provenance graph found for this change.")
                ));
                return output;
            }
            Err(e) => {
                output.push_str(&format!(
                    "{}\n",
                    hint(&format!("Failed to load provenance: {}", e))
                ));
                return output;
            }
        };

        for (_graph_hash, graph) in &graphs {
            output.push_str(&format!("{}\n", emphasis("=== Change Ledger ===")));
            output.push_str(&format!("  Session: {}\n", info(&graph.session_id)));
            output.push_str(&format!(
                "  Agent:   {} ({})\n",
                info(&graph.agent_display_name),
                hint(&graph.agent_vendor)
            ));
            output.push_str(&format!(
                "  Nodes:   {}  Edges: {}  Changes: {}\n",
                graph.node_count(),
                graph.edge_count(),
                graph.change_count()
            ));
            output.push('\n');

            // Display nodes
            for node in &graph.nodes {
                let kind_str = format!("{}", node.kind);
                let kind_styled = format!("{}", style(&kind_str).bold().cyan());
                let duration = node
                    .duration_ms
                    .map(|ms| format!(" ({}ms)", ms))
                    .unwrap_or_default();
                let tool = node.tool_name.as_deref().unwrap_or("");
                let tool_str = if tool.is_empty() {
                    String::new()
                } else {
                    format!(" {}", hint(&format!("[{}]", tool)))
                };

                // Extract a one-line detail string from the structured detail JSON
                let detail_str = node
                    .detail
                    .as_ref()
                    .and_then(|d| format_node_detail(d))
                    .map(|s| format!("\n      {}", hint(&s)))
                    .unwrap_or_default();

                output.push_str(&format!(
                    "  {} {} {}{}{}{}
",
                    kind_styled,
                    hint("\u{00bb}"),
                    node.summary,
                    tool_str,
                    hint(&duration),
                    detail_str,
                ));
            }
        }

        output
    }

    /// Format the change for JSON output.
    pub(crate) fn format_json(
        &self,
        change: &Change,
        hash: &Hash,
        sequence: Option<u64>,
        ledger: Vec<JsonChangeLedger>,
    ) -> String {
        let json_change =
            JsonChange::from_change_with_provenance(change, hash, sequence).with_ledger(ledger);
        serde_json::to_string_pretty(&json_change)
            .unwrap_or_else(|e| format!("{{\"error\": \"Failed to serialize: {}\"}}", e))
    }

    /// Load the change ledger(s) — the causal decision graph(s) from the
    /// `.provenance` file — for JSON output. Mirrors the `=== Change Ledger ===`
    /// section of the default text format. Missing/corrupt provenance is
    /// non-fatal: the ledger simply stays empty (and is omitted from the JSON).
    fn load_json_ledger(&self, hash: &Hash, repo: &Repository) -> Vec<JsonChangeLedger> {
        repo.find_provenance_for_change(hash)
            .unwrap_or_default()
            .iter()
            .map(|(graph_hash, graph)| JsonChangeLedger::from_graph(graph_hash, graph))
            .collect()
    }

    /// Print the change in the configured format.
    fn print_change(&self, change: &Change, hash: &Hash, sequence: Option<u64>, repo: &Repository) {
        let output = match self.format {
            ChangeFormat::Default => self.format_default(change, hash, sequence, repo),
            ChangeFormat::Short => self.format_short(change, hash, sequence),
            ChangeFormat::Json => {
                let ledger = self.load_json_ledger(hash, repo);
                self.format_json(change, hash, sequence, ledger)
            }
        };
        print!("{}", output);
    }
}

/// Extract a human-readable one-liner from a provenance node's detail JSON.
///
/// Returns `None` if the detail is empty or has no interesting fields.
fn format_node_detail(detail: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(detail).ok()?;
    let obj = v.as_object()?;

    // Command (bash/execution nodes)
    if let Some(cmd) = obj.get("command").and_then(|v| v.as_str()) {
        let truncated = if cmd.len() > 120 {
            format!("{}...", &cmd[..117])
        } else {
            cmd.to_string()
        };
        return Some(format!("$ {}", truncated));
    }

    // File path (read/write/edit nodes)
    if let Some(file) = obj
        .get("file")
        .or_else(|| obj.get("file_path"))
        .or_else(|| obj.get("target"))
        .and_then(|v| v.as_str())
    {
        let op = obj.get("operation").and_then(|v| v.as_str()).unwrap_or("");
        if op.is_empty() {
            return Some(file.to_string());
        }
        return Some(format!("{}: {}", op, file));
    }

    // Output summary (fallback for nodes with only output)
    if let Some(summary) = obj.get("output_summary").and_then(|v| v.as_str()) {
        let truncated = if summary.len() > 120 {
            format!("{}...", &summary[..117])
        } else {
            summary.to_string()
        };
        return Some(truncated);
    }

    None
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
            atomic_repository::RepositoryError::ViewNotFound { name } => {
                CliError::ViewNotFound { name }
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
pub(crate) fn truncate_string(s: &str, max_len: usize) -> String {
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
pub(crate) fn format_author(author: &Author) -> String {
    if let Some(ref email) = author.email {
        format!("{} <{}>", author.name, email)
    } else {
        author.name.clone()
    }
}

/// Count unique paths affected by hunks.
pub(crate) fn count_unique_paths<H>(hunks: &[GraphOp<H>]) -> usize {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HunkDisplaySummary {
    pub symbol: &'static str,
    pub path: String,
    pub info: String,
}

#[derive(Debug, Clone)]
struct HunkDisplayAggregate {
    symbol: &'static str,
    total: usize,
    infos: std::collections::BTreeMap<String, usize>,
}

pub(crate) fn hunk_display_summaries<H>(hunks: &[GraphOp<H>]) -> Vec<HunkDisplaySummary> {
    let mut by_path: std::collections::BTreeMap<String, HunkDisplayAggregate> =
        std::collections::BTreeMap::new();

    for graph_op in hunks {
        let (symbol, path) = hunk_symbol_and_path(graph_op);
        let info = hunk_atom_info(graph_op);
        let aggregate = by_path.entry(path).or_insert_with(|| HunkDisplayAggregate {
            symbol,
            total: 0,
            infos: std::collections::BTreeMap::new(),
        });
        aggregate.symbol = merge_hunk_symbols(aggregate.symbol, symbol);
        aggregate.total += 1;
        *aggregate.infos.entry(info).or_insert(0) += 1;
    }

    by_path
        .into_iter()
        .map(|(path, aggregate)| HunkDisplaySummary {
            symbol: aggregate.symbol,
            path,
            info: format_hunk_aggregate_info(aggregate.total, &aggregate.infos),
        })
        .collect()
}

fn merge_hunk_symbols(current: &'static str, next: &'static str) -> &'static str {
    if current == next {
        return current;
    }
    if current == "±" || next == "±" {
        return "±";
    }
    match (current, next) {
        ("+", "~") | ("~", "+") | ("+", "-") | ("-", "+") | ("~", "-") | ("-", "~") => "±",
        ("📁+", "📁-") | ("📁-", "📁+") => "±",
        _ => current,
    }
}

fn format_hunk_aggregate_info(
    total: usize,
    infos: &std::collections::BTreeMap<String, usize>,
) -> String {
    if total == 1 {
        return infos
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "(1 hunk)".to_string());
    }

    let details = infos
        .iter()
        .map(|(info, count)| format!("{}x {}", count, trim_hunk_info(info)))
        .collect::<Vec<_>>()
        .join("; ");
    format!("({} hunks: {})", total, details)
}

fn trim_hunk_info(info: &str) -> &str {
    info.strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(info)
}

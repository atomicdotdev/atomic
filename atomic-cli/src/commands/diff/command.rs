use super::output::*;
use super::*;

// Diff Command

/// Show changes between working copy and repository.
///
/// The `diff` command compares the current state of files in the working
/// copy against their recorded state in the repository, displaying the
/// differences in a human-readable format.
///
/// # Output Formats
///
/// - **Unified** (default): Traditional diff format with +/- markers
/// - **Stat**: Summary showing files and line counts
/// - **Name-only**: Just file paths
/// - **Name-status**: File paths with status indicators
///
/// # Algorithms
///
/// - **Myers** (default): Fast, finds minimal edit distance
/// - **Patience**: Better for code with moved blocks
#[derive(Parser, Debug, Clone)]
#[command(name = "diff")]
pub struct Diff {
    /// Specific files to diff (default: all modified tracked files).
    #[arg()]
    pub files: Vec<String>,

    /// Compare against a specific change hash or prefix.
    #[arg(short = 'c', long = "change")]
    pub change: Option<String>,

    /// Diff algorithm: myers or patience.
    #[arg(long, default_value = "myers")]
    pub algorithm: String,

    /// Number of context lines to show around changes.
    #[arg(long, default_value = "3", value_name = "N")]
    pub context: usize,

    /// Show only a stat summary.
    #[arg(long)]
    pub stat: bool,

    /// Disable colored output.
    #[arg(long)]
    pub no_color: bool,

    /// Show only names of changed files.
    #[arg(long)]
    pub name_only: bool,

    /// Show names with status indicators (M/A/D).
    #[arg(long)]
    pub name_status: bool,

    /// Short output format (equivalent to --name-status).
    #[arg(long)]
    pub short: bool,

    /// Include untracked files in the output.
    #[arg(long)]
    pub untracked: bool,

    /// Show staged changes (reserved for future use).
    #[arg(long, hide = true)]
    pub cached: bool,

    /// View to compare against.
    #[arg(long)]
    pub view: Option<String>,

    /// Enable token-level diff highlighting (CRDT-powered).
    ///
    /// Shows exactly which tokens changed within a line, not just
    /// that the line changed. Especially useful for code reviews.
    #[arg(long)]
    pub word_diff: bool,
}

impl Diff {
    /// Create a new Diff command with default settings.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            change: None,
            algorithm: "myers".to_string(),
            context: 3,
            stat: false,
            no_color: false,
            name_only: false,
            name_status: false,
            short: false,
            untracked: false,
            cached: false,
            view: None,
            word_diff: false,
        }
    }

    /// Builder: set files to diff.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Builder: set the change to compare against.
    pub fn with_change(mut self, change: impl Into<String>) -> Self {
        self.change = Some(change.into());
        self
    }

    /// Builder: set the diff algorithm.
    pub fn with_algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.algorithm = algorithm.into();
        self
    }

    /// Builder: set the number of context lines.
    pub fn with_context(mut self, context: usize) -> Self {
        self.context = context;
        self
    }

    /// Builder: set the stat flag.
    pub fn with_stat(mut self, stat: bool) -> Self {
        self.stat = stat;
        self
    }

    /// Builder: set the no-color flag.
    pub fn with_no_color(mut self, no_color: bool) -> Self {
        self.no_color = no_color;
        self
    }

    /// Builder: set the name-only flag.
    pub fn with_name_only(mut self, name_only: bool) -> Self {
        self.name_only = name_only;
        self
    }

    /// Builder: set the name-status flag.
    pub fn with_name_status(mut self, name_status: bool) -> Self {
        self.name_status = name_status;
        self
    }

    /// Builder: set the view to compare against.
    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    /// Builder: set the word-diff flag.
    pub fn with_word_diff(mut self, word_diff: bool) -> Self {
        self.word_diff = word_diff;
        self
    }

    /// Get the output format based on command flags.
    pub fn get_format(&self) -> DiffFormat {
        if self.name_only {
            DiffFormat::NameOnly
        } else if self.name_status || self.short {
            DiffFormat::NameStatus
        } else if self.stat {
            DiffFormat::Stat
        } else {
            DiffFormat::Unified
        }
    }

    /// Parse the algorithm string into an Algorithm enum.
    pub(crate) fn parse_algorithm(&self) -> CliResult<Algorithm> {
        self.algorithm
            .parse()
            .map_err(|_| CliError::InvalidArgument {
                message: format!(
                    "unknown diff algorithm '{}'. Valid options: myers, patience",
                    self.algorithm
                ),
            })
    }

    /// Create a DiffOutputConfig from the command settings.
    pub(crate) fn get_output_config(&self) -> DiffOutputConfig {
        DiffOutputConfig {
            context_lines: self.context,
            color: !self.no_color,
            format: self.get_format(),
            stat_width: 80,
            show_line_numbers: true,
            show_path_prefix: true,
            word_diff: self.word_diff,
        }
    }
}

impl Default for Diff {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Diff {
    /// Execute the diff command.
    ///
    /// This method:
    /// 1. Finds and opens the repository
    /// 2. Gets the status of the working copy
    /// 3. Computes diffs for modified files
    /// 4. Displays the diffs in the requested format
    fn run(&self) -> CliResult<()> {
        // Find the repository root
        let repo_root = find_repository_root()?;

        // Open the repository
        let repo =
            Repository::open_readonly(&repo_root).map_err(|e| CliError::InvalidRepository {
                reason: e.to_string(),
            })?;

        // Parse algorithm
        let algorithm = self.parse_algorithm()?;

        // Get output configuration
        let config = self.get_output_config();

        // If --change is specified, show the content of that specific change
        if let Some(change_ref) = &self.change {
            return self.show_change_diff(&repo, change_ref, &config);
        }

        // Get status to find modified files
        let status_options = StatusOptions::default();
        let status = repo
            .status(status_options)
            .map_err(|e| CliError::Internal(e.into()))?;

        // Collect files to diff
        let files_to_diff: Vec<_> = if self.files.is_empty() {
            // Diff all modified and added files
            let mut entries: Vec<_> = status
                .modified()
                .chain(status.added())
                .chain(status.deleted())
                .map(|e| (e.path().to_path_buf(), e.status()))
                .collect();

            // Include untracked files if --untracked flag is set
            if self.untracked {
                entries.extend(
                    status
                        .untracked()
                        .map(|e| (e.path().to_path_buf(), e.status())),
                );
            }

            entries
        } else {
            // Diff only specified files
            self.files
                .iter()
                .filter_map(|path| {
                    let path_buf = PathBuf::from(path);
                    // Find the file in status
                    status
                        .entries()
                        .iter()
                        .find(|e| e.path() == path_buf)
                        .map(|e| (e.path().to_path_buf(), e.status()))
                })
                .collect()
        };

        // Check if there are any changes
        if files_to_diff.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Compute diffs for each file
        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for (path, file_status) in &files_to_diff {
            let path_str = path.display().to_string();
            let _change_status = FileChangeStatus::from(*file_status);

            match file_status {
                FileStatus::Deleted => {
                    // For deleted files, retrieve the old content from the graph
                    let old_content = match repo.get_file_content_on_view(path, repo.current_view())
                    {
                        Ok(Some(content)) => content,
                        Ok(None) => Vec::new(),
                        Err(_) => Vec::new(),
                    };

                    let mut diff = FileDiff::deleted(&path_str);

                    if !old_content.is_empty() {
                        let lines: Vec<_> = old_content.split(|&b| b == b'\n').collect();
                        let line_count = lines.len();

                        // Create a single hunk with all deleted content
                        let mut graph_op = DiffHunk::new(1, line_count, 0, 0);
                        for (i, line_bytes) in lines.iter().enumerate() {
                            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
                            graph_op.add_line(HunkLine::removed(line_content, i + 1));
                        }
                        diff.add_hunk(graph_op);
                        diff.stats = FileDiffStats::deleted(&path_str, line_count);
                    } else {
                        diff.stats = FileDiffStats::deleted(&path_str, 0);
                    }

                    stats.add_file(diff.stats.clone());
                    file_diffs.push(diff);
                }
                FileStatus::Untracked => {
                    // For untracked files, show as added content
                    let full_path = repo_root.join(path);
                    match std::fs::read(&full_path) {
                        Ok(content) => {
                            let lines: Vec<_> = content.split(|&b| b == b'\n').collect();
                            let line_count = if content.is_empty() { 0 } else { lines.len() };

                            let mut diff = FileDiff::new(&path_str, FileChangeStatus::Untracked);

                            // Create a single hunk with all new content
                            if !content.is_empty() {
                                let mut graph_op = DiffHunk::new(0, 0, 1, line_count);
                                for (i, line_bytes) in lines.iter().enumerate() {
                                    let line_content =
                                        String::from_utf8_lossy(line_bytes).into_owned();
                                    graph_op.add_line(HunkLine::added(line_content, i + 1));
                                }
                                diff.add_hunk(graph_op);
                            }

                            diff.stats = FileDiffStats::added(&path_str, line_count);
                            stats.add_file(diff.stats.clone());
                            file_diffs.push(diff);
                        }
                        Err(_) => {
                            // File might not be readable, skip it
                            continue;
                        }
                    }
                }
                FileStatus::Added => {
                    // For added files, read the new content
                    let full_path = repo_root.join(path);
                    match std::fs::read(&full_path) {
                        Ok(content) => {
                            let lines: Vec<_> = content.split(|&b| b == b'\n').collect();
                            let line_count = if content.is_empty() { 0 } else { lines.len() };

                            let mut diff = FileDiff::added(&path_str);

                            // Create a single hunk with all new content
                            if !content.is_empty() {
                                let mut graph_op = DiffHunk::new(0, 0, 1, line_count);
                                for (i, line_bytes) in lines.iter().enumerate() {
                                    let line_content =
                                        String::from_utf8_lossy(line_bytes).into_owned();
                                    graph_op.add_line(HunkLine::added(line_content, i + 1));
                                }
                                diff.add_hunk(graph_op);
                            }

                            diff.stats = FileDiffStats::added(&path_str, line_count);
                            stats.add_file(diff.stats.clone());
                            file_diffs.push(diff);
                        }
                        Err(_) => {
                            // File might not be readable, skip it
                            continue;
                        }
                    }
                }
                FileStatus::Modified => {
                    // For modified files, compute the actual diff
                    let full_path = repo_root.join(path);

                    // Read current (new) content from working copy
                    let new_content = match std::fs::read(&full_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    // Retrieve the old (recorded) content from the graph.
                    let old_content = match repo.get_file_content_on_view(path, repo.current_view())
                    {
                        Ok(Some(content)) => content,
                        Ok(None) => Vec::new(), // No recorded content (newly tracked)
                        Err(_) => Vec::new(),   // Error retrieving - treat as new
                    };

                    // Compute diff between old (recorded) and new (working copy)
                    let diff_result = diff_text(&old_content, &new_content, algorithm);

                    // Convert to FileDiff
                    let mut file_diff = FileDiff::modified(&path_str);

                    // Build hunks from diff result
                    if !diff_result.is_unchanged() {
                        let new_lines: Vec<_> = new_content.split(|&b| b == b'\n').collect();
                        let old_lines: Vec<_> = old_content.split(|&b| b == b'\n').collect();

                        // Create hunks with context
                        let hunks = build_hunks_from_diff(
                            &diff_result,
                            &old_lines,
                            &new_lines,
                            config.context_lines,
                        );
                        for graph_op in hunks {
                            file_diff.add_hunk(graph_op);
                        }
                    }

                    file_diff.compute_stats();
                    stats.add_file(file_diff.stats.clone());
                    file_diffs.push(file_diff);
                }
                _ => {
                    // Other statuses - skip for now
                    continue;
                }
            }
        }

        // Print in the appropriate format
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, &config),
            DiffFormat::Stat => self.print_stat(&stats, &config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, &config),
        }
    }
}

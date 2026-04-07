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
    /// Specific files to diff.
    ///
    /// If not provided, shows diff for all modified tracked files.
    /// Paths are relative to the repository root.
    #[arg()]
    pub files: Vec<String>,

    /// Compare against a specific change.
    ///
    /// Shows the diff between the specified change and the working copy,
    /// rather than comparing against the current recorded state.
    #[arg(short = 'c', long = "change")]
    pub change: Option<String>,

    /// Diff algorithm to use.
    ///
    /// - myers: Standard LCS-based diff (default, fast)
    /// - patience: Better for code with repeated patterns
    #[arg(long, default_value = "myers")]
    pub algorithm: String,

    /// Number of context lines to show.
    ///
    /// Context lines are unchanged lines shown around changes to
    /// provide context. More lines make the diff easier to read
    /// but longer.
    #[arg(long, default_value = "3", value_name = "N")]
    pub context: usize,

    /// Show only a stat summary.
    ///
    /// Instead of showing the full diff, shows a summary with file
    /// names and insertion/deletion counts.
    #[arg(long)]
    pub stat: bool,

    /// Disable colored output.
    ///
    /// By default, diff output is colored for readability. Use this
    /// flag to disable colors (useful for piping to files).
    #[arg(long)]
    pub no_color: bool,

    /// Show only names of changed files.
    ///
    /// Lists just the file paths without any diff content.
    #[arg(long)]
    pub name_only: bool,

    /// Show names with status indicators.
    ///
    /// Lists file paths prefixed with their status (M/A/D).
    #[arg(long)]
    pub name_status: bool,

    /// Short output format (equivalent to --name-status).
    ///
    /// Shows file paths with their status indicator (M/A/D/U).
    /// This is a convenience alias for --name-status, commonly
    /// used for scripting and integration with other tools.
    ///
    /// Output format:
    /// - M path/to/file  (modified)
    /// - A path/to/file  (added/tracked)
    /// - D path/to/file  (deleted)
    /// - U path/to/file  (untracked, with --untracked)
    #[arg(long)]
    pub short: bool,

    /// Include untracked files in the output.
    ///
    /// By default, only tracked files are shown. Use this flag
    /// to also include files that haven't been added to tracking.
    /// Untracked files are shown with status 'U' in short/name-status
    /// format, or as added files in other formats.
    #[arg(long)]
    pub untracked: bool,

    /// Show staged changes (reserved for future use).
    ///
    /// This option is reserved for future implementation of a
    /// staging area feature.
    #[arg(long, hide = true)]
    pub cached: bool,

    /// Stack to compare against.
    ///
    /// By default, compares against the current stack. Use this
    /// to compare against a different stack.
    #[arg(long)]
    pub stack: Option<String>,

    /// Enable token-level diff highlighting (CRDT-powered).
    ///
    /// Shows exactly which tokens changed within a line, not just
    /// that the line changed. This uses the same tokenization engine
    /// as the CRDT model, recognizing:
    ///
    /// - Keywords, identifiers, operators
    /// - String literals, numbers, comments
    /// - Whitespace and punctuation
    ///
    /// Highlighting:
    /// - Deleted tokens: bright red with underline
    /// - Added tokens: bright green with underline
    ///
    /// This is especially useful for code reviews to quickly identify
    /// variable renames, parameter changes, and string modifications.
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
            stack: None,
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

    /// Builder: set the stack to compare against.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
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

    /// Print the diff in unified format.
    fn print_unified(&self, file_diffs: &[FileDiff], config: &DiffOutputConfig) -> CliResult<()> {
        for file_diff in file_diffs {
            // Print file header
            let old_path = if config.show_path_prefix {
                format!("a/{}", file_diff.old_path)
            } else {
                file_diff.old_path.clone()
            };
            let new_path = if config.show_path_prefix {
                format!("b/{}", file_diff.new_path)
            } else {
                file_diff.new_path.clone()
            };

            // Format line stats (e.g., "+2 -1") with colors
            let line_stats = if file_diff.stats.insertions > 0 || file_diff.stats.deletions > 0 {
                let ins = if file_diff.stats.insertions > 0 {
                    format!("+{}", file_diff.stats.insertions)
                } else {
                    String::new()
                };
                let del = if file_diff.stats.deletions > 0 {
                    format!("-{}", file_diff.stats.deletions)
                } else {
                    String::new()
                };
                (ins, del)
            } else {
                (String::new(), String::new())
            };

            if config.color {
                // Build colored line stats
                let colored_stats = if !line_stats.0.is_empty() || !line_stats.1.is_empty() {
                    let ins_colored = if !line_stats.0.is_empty() {
                        added(&line_stats.0).to_string()
                    } else {
                        String::new()
                    };
                    let del_colored = if !line_stats.1.is_empty() {
                        deleted(&line_stats.1).to_string()
                    } else {
                        String::new()
                    };
                    if !ins_colored.is_empty() && !del_colored.is_empty() {
                        format!(" ({} {})", ins_colored, del_colored)
                    } else if !ins_colored.is_empty() {
                        format!(" ({})", ins_colored)
                    } else {
                        format!(" ({})", del_colored)
                    }
                } else {
                    String::new()
                };
                println!(
                    "{}{}",
                    emphasis(&format!("diff --atomic {} {}", old_path, new_path)),
                    colored_stats
                );
                println!("{}", deleted(&format!("--- {}", old_path)));
                println!("{}", added(&format!("+++ {}", new_path)));
            } else {
                // Non-colored output
                let plain_stats = if !line_stats.0.is_empty() || !line_stats.1.is_empty() {
                    if !line_stats.0.is_empty() && !line_stats.1.is_empty() {
                        format!(" ({} {})", line_stats.0, line_stats.1)
                    } else if !line_stats.0.is_empty() {
                        format!(" ({})", line_stats.0)
                    } else {
                        format!(" ({})", line_stats.1)
                    }
                } else {
                    String::new()
                };
                println!("diff --atomic {} {}{}", old_path, new_path, plain_stats);
                println!("--- {}", old_path);
                println!("+++ {}", new_path);
            }

            // Handle binary files
            if file_diff.is_binary {
                println!("Binary files differ");
                continue;
            }

            // Print each graph_op
            for graph_op in &file_diff.hunks {
                // Print graph_op header
                if config.color {
                    println!("{}", info(&graph_op.header()));
                } else {
                    println!("{}", graph_op.header());
                }

                // Print graph_op lines with optional word-level highlighting
                // First, collect consecutive removed and added lines to pair them correctly
                let mut i = 0;
                while i < graph_op.lines.len() {
                    let line = &graph_op.lines[i];

                    // Check if we can do word-level diff
                    // For Replace operations, we may have multiple removed lines followed by multiple added lines
                    // We need to collect them all and pair them by position
                    if config.color && line.status == LineStatus::Removed {
                        // Collect all consecutive removed lines
                        let mut removed_lines: Vec<&HunkLine> = vec![line];
                        let mut j = i + 1;
                        while j < graph_op.lines.len()
                            && graph_op.lines[j].status == LineStatus::Removed
                        {
                            removed_lines.push(&graph_op.lines[j]);
                            j += 1;
                        }

                        // Collect all consecutive added lines that follow
                        let mut added_lines: Vec<&HunkLine> = Vec::new();
                        while j < graph_op.lines.len()
                            && graph_op.lines[j].status == LineStatus::Added
                        {
                            added_lines.push(&graph_op.lines[j]);
                            j += 1;
                        }

                        // If we have both removed and added lines, pair them for word-level diff
                        if !added_lines.is_empty() {
                            let pairs = removed_lines.len().min(added_lines.len());

                            // Process paired lines with word-level highlighting
                            for k in 0..pairs {
                                let removed_line = removed_lines[k];
                                let added_line = added_lines[k];

                                // Use semantic diff for better token-level highlighting
                                let old_content = removed_line.content.as_bytes();
                                let new_content = added_line.content.as_bytes();

                                // Compute semantic diff for precise token boundaries
                                let sem_diff = semantic_diff(old_content, new_content);

                                let mut used_semantic = false;
                                #[allow(clippy::collapsible_match)]
                                if let Some(change) = sem_diff.changes().first() {
                                    if let LineChange::Modified { token_changes, .. } = change {
                                        // Print old line with semantic token highlighting
                                        let old_num_str = if config.show_line_numbers {
                                            format!(
                                                "{:>4} {:>4} ",
                                                removed_line
                                                    .old_line_num
                                                    .map(|n| n.to_string())
                                                    .unwrap_or_default(),
                                                ""
                                            )
                                        } else {
                                            String::new()
                                        };
                                        print!(
                                            "{}",
                                            deleted(&format!(
                                                "{}{}",
                                                old_num_str,
                                                removed_line.prefix()
                                            ))
                                        );
                                        print_semantic_word_diff_line(token_changes, true);
                                        println!();

                                        // Print new line with semantic token highlighting
                                        let new_num_str = if config.show_line_numbers {
                                            format!(
                                                "{:>4} {:>4} ",
                                                "",
                                                added_line
                                                    .new_line_num
                                                    .map(|n| n.to_string())
                                                    .unwrap_or_default()
                                            )
                                        } else {
                                            String::new()
                                        };
                                        print!(
                                            "{}",
                                            added(&format!(
                                                "{}{}",
                                                new_num_str,
                                                added_line.prefix()
                                            ))
                                        );
                                        print_semantic_word_diff_line(token_changes, false);
                                        println!();

                                        used_semantic = true;
                                    }
                                }

                                if !used_semantic {
                                    // Fallback to inline diff if semantic diff didn't work
                                    let inline_diff = compute_inline_diff(old_content, new_content);

                                    // Print old line with word-level highlighting
                                    let old_num_str = if config.show_line_numbers {
                                        format!(
                                            "{:>4} {:>4} ",
                                            removed_line
                                                .old_line_num
                                                .map(|n| n.to_string())
                                                .unwrap_or_default(),
                                            ""
                                        )
                                    } else {
                                        String::new()
                                    };
                                    print!(
                                        "{}",
                                        deleted(&format!(
                                            "{}{}",
                                            old_num_str,
                                            removed_line.prefix()
                                        ))
                                    );
                                    print_word_diff_line(
                                        old_content,
                                        inline_diff.old_hunks(),
                                        true,
                                    );
                                    println!();

                                    // Print new line with word-level highlighting
                                    let new_num_str = if config.show_line_numbers {
                                        format!(
                                            "{:>4} {:>4} ",
                                            "",
                                            added_line
                                                .new_line_num
                                                .map(|n| n.to_string())
                                                .unwrap_or_default()
                                        )
                                    } else {
                                        String::new()
                                    };
                                    print!(
                                        "{}",
                                        added(&format!("{}{}", new_num_str, added_line.prefix()))
                                    );
                                    print_word_diff_line(
                                        new_content,
                                        inline_diff.new_hunks(),
                                        false,
                                    );
                                    println!();
                                }
                            }

                            // Print any remaining unpaired removed lines
                            for removed_line in removed_lines.iter().skip(pairs) {
                                let removed_line = *removed_line;
                                let line_num_str = if config.show_line_numbers {
                                    format!(
                                        "{:>4} {:>4} ",
                                        removed_line
                                            .old_line_num
                                            .map(|n| n.to_string())
                                            .unwrap_or_default(),
                                        ""
                                    )
                                } else {
                                    String::new()
                                };
                                let formatted = format!(
                                    "{}{}{}",
                                    line_num_str,
                                    removed_line.prefix(),
                                    removed_line.content
                                );
                                println!("{}", deleted(&formatted));
                            }

                            // Print any remaining unpaired added lines
                            for added_line in added_lines.iter().skip(pairs) {
                                let added_line = *added_line;
                                let line_num_str = if config.show_line_numbers {
                                    format!(
                                        "{:>4} {:>4} ",
                                        "",
                                        added_line
                                            .new_line_num
                                            .map(|n| n.to_string())
                                            .unwrap_or_default()
                                    )
                                } else {
                                    String::new()
                                };
                                let formatted = format!(
                                    "{}{}{}",
                                    line_num_str,
                                    added_line.prefix(),
                                    added_line.content
                                );
                                println!("{}", added(&formatted));
                            }

                            // Skip all processed lines
                            i = j;
                            continue;
                        }
                    }

                    // Standard line output (no word-level diff)
                    let line_num_str = if config.show_line_numbers {
                        match line.status {
                            LineStatus::Added => {
                                format!(
                                    "{:>4} {:>4} ",
                                    "",
                                    line.new_line_num.map(|n| n.to_string()).unwrap_or_default()
                                )
                            }
                            LineStatus::Removed => {
                                format!(
                                    "{:>4} {:>4} ",
                                    line.old_line_num.map(|n| n.to_string()).unwrap_or_default(),
                                    ""
                                )
                            }
                            LineStatus::Unchanged => {
                                format!(
                                    "{:>4} {:>4} ",
                                    line.old_line_num.map(|n| n.to_string()).unwrap_or_default(),
                                    line.new_line_num.map(|n| n.to_string()).unwrap_or_default()
                                )
                            }
                        }
                    } else {
                        String::new()
                    };
                    let formatted = format!("{}{}{}", line_num_str, line.prefix(), line.content);
                    if config.color {
                        match line.status {
                            LineStatus::Added => println!("{}", added(&formatted)),
                            LineStatus::Removed => println!("{}", deleted(&formatted)),
                            LineStatus::Unchanged => println!("{}", formatted),
                        }
                    } else {
                        println!("{}", formatted);
                    }
                    i += 1;
                }
            }
        }

        Ok(())
    }

    /// Print the diff in stat format.
    fn print_stat(&self, stats: &DiffStats, config: &DiffOutputConfig) -> CliResult<()> {
        if !stats.has_changes() {
            return Ok(());
        }

        let max_path_len = stats.max_path_length();
        let max_changes = stats.max_change_count();
        let graph_width = cmp::min(config.stat_width, max_changes);

        for file_stats in stats.iter() {
            let path = &file_stats.path;
            let padding = max_path_len - path.len();
            let total = file_stats.total_changes();

            // Calculate graph
            let graph = if total > 0 && graph_width > 0 {
                let scale = if max_changes > graph_width {
                    graph_width as f64 / max_changes as f64
                } else {
                    1.0
                };
                let plus_count = ((file_stats.insertions as f64 * scale).round() as usize)
                    .max(if file_stats.insertions > 0 { 1 } else { 0 });
                let minus_count = ((file_stats.deletions as f64 * scale).round() as usize)
                    .max(if file_stats.deletions > 0 { 1 } else { 0 });
                format!("{}{}", "+".repeat(plus_count), "-".repeat(minus_count))
            } else {
                String::new()
            };

            if config.color {
                let plus_part = "+".repeat(file_stats.insertions.min(graph_width));
                let minus_part = "-".repeat(file_stats.deletions.min(graph_width));
                println!(
                    " {} {} | {} {}{}",
                    style_path(path),
                    " ".repeat(padding),
                    total,
                    added(&plus_part),
                    deleted(&minus_part)
                );
            } else {
                println!(" {} {} | {} {}", path, " ".repeat(padding), total, graph);
            }
        }

        // Print summary
        let files_text = if stats.file_count() == 1 {
            "file"
        } else {
            "files"
        };
        let ins_text = if stats.total_insertions() == 1 {
            "insertion"
        } else {
            "insertions"
        };
        let del_text = if stats.total_deletions() == 1 {
            "deletion"
        } else {
            "deletions"
        };

        println!(
            " {} {} changed, {} {}(+), {} {}(-)",
            stats.file_count(),
            files_text,
            stats.total_insertions(),
            ins_text,
            stats.total_deletions(),
            del_text
        );

        Ok(())
    }

    /// Print file names only.
    fn print_name_only(&self, file_diffs: &[FileDiff]) -> CliResult<()> {
        for file_diff in file_diffs {
            println!("{}", file_diff.display_path());
        }
        Ok(())
    }

    /// Print file names with status.
    fn print_name_status(
        &self,
        file_diffs: &[FileDiff],
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        for file_diff in file_diffs {
            let status_char = file_diff.status.status_char();
            let path = file_diff.display_path();

            if config.color {
                let status_str = status_char.to_string();
                let styled_status = match file_diff.status {
                    FileChangeStatus::Added => added(&status_str),
                    FileChangeStatus::Deleted => deleted(&status_str),
                    FileChangeStatus::Modified => modified(&status_str),
                    _ => info(&status_str),
                };
                println!("{}  {}", styled_status, style_path(path));
            } else {
                println!("{}  {}", status_char, path);
            }
        }
        Ok(())
    }

    /// Print a message when there are no changes.
    fn print_no_changes(&self) {
        print_info("No changes detected");
    }

    /// Show the diff for a specific change by hash or prefix.
    ///
    /// This displays the content introduced by the change using state-based
    /// content retrieval. For each file modified by the change, we retrieve:
    /// - The file content BEFORE the change was applied (parent state)
    /// - The file content AFTER the change was applied (current state)
    ///
    /// Then we compute a proper diff between the two states, with optional
    /// word-level highlighting for code review.
    ///
    /// # State-Based Content Retrieval
    ///
    /// ```text
    /// Stack History:
    ///   seq 0    seq 1    seq 2    seq 3    seq 4
    ///   ──┬────────┬────────┬────────┬────────┬──
    ///     │        │        │        │        │
    ///   [A]      [B]      [C]      [D]      [E]
    ///                               ↑
    ///                         change_hash = D
    ///
    /// Before state: content after applying [A, B, C]
    /// After state:  content after applying [A, B, C, D]
    /// Diff: shows exactly what change D modified
    /// ```
    ///
    /// # Algorithm
    ///
    /// 1. Resolve the change hash from reference (full hash or prefix)
    /// 2. Load the change to get the list of affected files
    /// 3. For each file:
    ///    a. Get content BEFORE the change (using parent state filter)
    ///    b. Get content AFTER the change (using current state filter)
    ///    c. Compute diff between before/after
    ///    d. Optionally apply word-level highlighting
    /// 4. Display in the requested format
    fn show_change_diff(
        &self,
        repo: &Repository,
        change_ref: &str,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        // Resolve the change reference (full hash or prefix)
        let hash = self.resolve_change_ref(repo, change_ref)?;

        // Load the change
        let change =
            repo.change_store()
                .load_change(&hash)
                .map_err(|_e| CliError::ChangeNotFound {
                    hash: change_ref.to_string(),
                })?;

        // Check if change has semantic layer (file_ops)
        if change.has_file_ops() {
            // Use the semantic layer for human-readable diff
            return self.show_change_diff_from_file_ops(&change, &hash, config);
        }

        // Fallback: compute diff from content (legacy path)
        self.show_change_diff_computed(repo, &change, &hash, config)
    }

    /// Show diff using the semantic layer (FileOps).
    ///
    /// This is the preferred path - it displays line-level and token-level
    /// changes directly from the stored CRDT operations, without recomputing.
    fn show_change_diff_from_file_ops(
        &self,
        change: &Change,
        change_hash: &Hash,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        let file_ops = change.file_ops();

        if file_ops.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for ops in file_ops {
            let file_path = ops.path();
            let trunk_op = ops.trunk_op();
            let line_ops = ops.line_ops();

            // Determine file change status from trunk operation
            let change_status = match trunk_op {
                Some(TrunkOp::Create { .. }) => FileChangeStatus::Added,
                Some(TrunkOp::Delete { .. }) => FileChangeStatus::Deleted,
                Some(TrunkOp::Move { .. }) => FileChangeStatus::Renamed,
                Some(TrunkOp::Undelete { .. }) => FileChangeStatus::Modified,
                None => FileChangeStatus::Modified,
            };

            let mut file_diff = match change_status {
                FileChangeStatus::Added => FileDiff::added(file_path),
                FileChangeStatus::Deleted => FileDiff::deleted(file_path),
                FileChangeStatus::Renamed => FileDiff::modified(file_path),
                _ => FileDiff::modified(file_path),
            };

            // Build hunks from line operations
            let mut insertions = 0usize;
            let mut deletions = 0usize;
            let mut new_line_num = 1usize;
            let mut old_line_num = 1usize;

            // Group consecutive operations into hunks
            if !line_ops.is_empty() {
                let mut current_hunk = DiffHunk::new(old_line_num, 0, new_line_num, 0);

                for line_op in line_ops {
                    match line_op.operation() {
                        BranchOp::Insert { content, .. } => {
                            // Reconstruct line content from leaf operations
                            let line_content = Self::reconstruct_line_from_leaf_ops(content);
                            // Use stored line number if available, otherwise use counter
                            let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                            current_hunk.add_line(HunkLine::added(line_content, line_num));
                            new_line_num = line_num + 1;
                            insertions += 1;
                            current_hunk.new_count += 1;
                        }
                        BranchOp::Delete { content, .. } => {
                            // Reconstruct deleted line content from stored leaf operations
                            let line_content = if content.is_empty() {
                                String::from("<deleted line>")
                            } else {
                                Self::reconstruct_line_from_leaf_ops(content)
                            };
                            // Use stored line number if available, otherwise use counter
                            let line_num = line_op.old_line_num().unwrap_or(old_line_num);
                            current_hunk.add_line(HunkLine::removed(line_content, line_num));
                            old_line_num = line_num + 1;
                            deletions += 1;
                            current_hunk.old_count += 1;
                        }
                        BranchOp::Modify {
                            old_content,
                            new_content,
                            ..
                        } => {
                            // A Modify carries both old and new content.
                            // Emit them as adjacent removed + added lines
                            // so print_unified can pair them for word-level
                            // highlighting.
                            let old_line_content = if old_content.is_empty() {
                                String::from("<modified line>")
                            } else {
                                Self::reconstruct_line_from_leaf_ops(old_content)
                            };
                            let new_line_content =
                                Self::reconstruct_line_from_leaf_ops(new_content);

                            let old_ln = line_op.old_line_num().unwrap_or(old_line_num);
                            let new_ln = line_op.new_line_num().unwrap_or(new_line_num);

                            current_hunk.add_line(HunkLine::removed(old_line_content, old_ln));
                            current_hunk.add_line(HunkLine::added(new_line_content, new_ln));

                            old_line_num = old_ln + 1;
                            new_line_num = new_ln + 1;
                            deletions += 1;
                            insertions += 1;
                            current_hunk.old_count += 1;
                            current_hunk.new_count += 1;
                        }
                        BranchOp::Restore { .. } => {
                            // Restore is like an add for display purposes
                            let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                            current_hunk.add_line(HunkLine::added(
                                String::from("<restored line>"),
                                line_num,
                            ));
                            new_line_num = line_num + 1;
                            insertions += 1;
                            current_hunk.new_count += 1;
                        }
                    }
                }

                if current_hunk.has_changes() {
                    file_diff.add_hunk(current_hunk);
                }
            }

            // Set stats based on change type
            file_diff.stats = match change_status {
                FileChangeStatus::Added => FileDiffStats::added(file_path, insertions),
                FileChangeStatus::Deleted => FileDiffStats::deleted(file_path, deletions),
                _ => FileDiffStats::modified(file_path, insertions, deletions),
            };

            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }

        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Print change header information
        if config.format == DiffFormat::Unified {
            self.print_change_header(change, change_hash, config);
        }

        // Print in the appropriate format
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, config),
            DiffFormat::Stat => self.print_stat(&stats, config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, config),
        }
    }

    /// Reconstruct a line's text content from its leaf operations.
    fn reconstruct_line_from_leaf_ops(leaf_ops: &[LeafOp]) -> String {
        let mut line = String::new();
        for leaf_op in leaf_ops {
            match leaf_op {
                LeafOp::Insert { content, .. } => {
                    if let Ok(text) = std::str::from_utf8(content) {
                        line.push_str(text);
                    }
                }
                LeafOp::Replace { new_content, .. } => {
                    if let Ok(text) = std::str::from_utf8(new_content) {
                        line.push_str(text);
                    }
                }
                LeafOp::Delete { .. } | LeafOp::Restore { .. } => {
                    // These don't add content to the line
                }
            }
        }
        line
    }

    /// Show diff by computing from content (legacy fallback).
    ///
    /// Used when a change doesn't have file_ops (old changes or graph-only changes).
    fn show_change_diff_computed(
        &self,
        repo: &Repository,
        change: &Change,
        hash: &Hash,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        use atomic_repository::get_files_in_change;

        // Get all files modified by this change
        let modified_files = get_files_in_change(change);

        if modified_files.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Parse algorithm for diffing
        let algorithm = self.parse_algorithm()?;

        // Compute diffs for each file using state-based content retrieval
        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for file_path in &modified_files {
            // Get content BEFORE the change was applied
            let before_content = match repo.get_file_content_before_change(file_path, hash) {
                Ok(content) => content.unwrap_or_default(),
                Err(_) => Vec::new(),
            };

            // Get content AFTER the change was applied
            let after_content = match repo.get_file_content_after_change(file_path, hash) {
                Ok(content) => content.unwrap_or_default(),
                Err(_) => Vec::new(),
            };

            // Determine the type of change based on before/after content
            let file_diff = match (before_content.is_empty(), after_content.is_empty()) {
                // File was added (no content before, has content after)
                (true, false) => {
                    let mut diff = FileDiff::added(file_path);
                    let lines: Vec<_> = after_content.split(|&b| b == b'\n').collect();
                    let line_count = lines.len();

                    if !after_content.is_empty() {
                        let mut graph_op = DiffHunk::new(0, 0, 1, line_count);
                        for (i, line_bytes) in lines.iter().enumerate() {
                            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
                            graph_op.add_line(HunkLine::added(line_content, i + 1));
                        }
                        diff.add_hunk(graph_op);
                    }

                    diff.stats = FileDiffStats::added(file_path, line_count);
                    diff
                }

                // File was deleted (has content before, no content after)
                (false, true) => {
                    let mut diff = FileDiff::deleted(file_path);
                    let lines: Vec<_> = before_content.split(|&b| b == b'\n').collect();
                    let line_count = lines.len();

                    if !before_content.is_empty() {
                        let mut graph_op = DiffHunk::new(1, line_count, 0, 0);
                        for (i, line_bytes) in lines.iter().enumerate() {
                            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
                            graph_op.add_line(HunkLine::removed(line_content, i + 1));
                        }
                        diff.add_hunk(graph_op);
                    }

                    diff.stats = FileDiffStats::deleted(file_path, line_count);
                    diff
                }

                // File was modified (has content both before and after)
                (false, false) => {
                    let mut diff = FileDiff::modified(file_path);

                    // Compute diff between old (before) and new (after) content
                    let diff_result = diff_text(&before_content, &after_content, algorithm);

                    if !diff_result.is_unchanged() {
                        let old_lines: Vec<_> = before_content.split(|&b| b == b'\n').collect();
                        let new_lines: Vec<_> = after_content.split(|&b| b == b'\n').collect();

                        // Build hunks with context
                        let hunks = build_hunks_from_diff(
                            &diff_result,
                            &old_lines,
                            &new_lines,
                            config.context_lines,
                        );
                        for graph_op in hunks {
                            diff.add_hunk(graph_op);
                        }
                    }

                    diff.compute_stats();
                    diff
                }

                // No content at all (shouldn't happen for files in change)
                (true, true) => {
                    continue; // Skip files with no content
                }
            };

            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }

        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Print change header information
        if config.format == DiffFormat::Unified {
            self.print_change_header(change, hash, config);
        }

        // Print in the appropriate format
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, config),
            DiffFormat::Stat => self.print_stat(&stats, config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, config),
        }
    }

    /// Print header information for a change diff.
    ///
    /// Displays the change hash, message, author, and timestamp before
    /// showing the actual diff content.
    fn print_change_header(&self, change: &Change, change_hash: &Hash, config: &DiffOutputConfig) {
        let header = &change.hashed.header;
        let hash_str = change_hash.to_base32();
        let display_hash = hash_str[..DEFAULT_HASH_LENGTH.min(hash_str.len())].to_string();

        // Print change identifier
        if config.color {
            println!("{} {}", emphasis("change"), hash(&display_hash));
        } else {
            println!("change {}", display_hash);
        }

        // Print author(s)
        for author in header.authors.iter() {
            let author_str = if let Some(ref email) = author.email {
                format!("{} <{}>", author.name, email)
            } else {
                author.name.clone()
            };
            if config.color {
                println!("Author: {}", info(&author_str));
            } else {
                println!("Author: {}", author_str);
            }
        }

        // Print timestamp
        let timestamp = header.timestamp.format("%Y-%m-%d %H:%M:%S %Z").to_string();
        if config.color {
            println!("Date:   {}", info(&timestamp));
        } else {
            println!("Date:   {}", timestamp);
        }

        // Print message
        println!();
        if config.color {
            println!("    {}", emphasis(&header.message));
        } else {
            println!("    {}", header.message);
        }

        // Print description if present
        if let Some(ref desc) = header.description {
            println!();
            for line in desc.lines() {
                println!("    {}", line);
            }
        }

        println!();
    }

    /// Resolve a change reference (full hash or prefix) to a full hash.
    fn resolve_change_ref(&self, repo: &Repository, change_ref: &str) -> CliResult<Hash> {
        // Try to parse as a full hash first
        if let Some(hash) = Hash::from_base32(change_ref.as_bytes()) {
            if repo.has_change(&hash) {
                return Ok(hash);
            }
        }

        // Search for matching changes by prefix
        let mut matches: Vec<Hash> = Vec::new();
        let prefix_upper = change_ref.to_uppercase();

        for result in repo.iter_changes() {
            let hash = result.map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;
            let hash_str = hash.to_base32();
            if hash_str.starts_with(&prefix_upper) {
                matches.push(hash);
            }
        }

        match matches.len() {
            0 => Err(CliError::ChangeNotFound {
                hash: change_ref.to_string(),
            }),
            1 => Ok(matches[0]),
            _ => {
                let match_list: Vec<String> = matches.iter().map(|h| h.to_base32()).collect();
                Err(CliError::AmbiguousHash {
                    hash: format!("{} (matches: {})", change_ref, match_list.join(", ")),
                })
            }
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
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found
    /// - The repository cannot be opened
    /// - Status computation fails
    /// - Diff computation fails
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

                        // Create a single graph_op with all deleted content
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
                    // For untracked files in short/name-status format, just show status
                    // For other formats, show as added content
                    let full_path = repo_root.join(path);
                    match std::fs::read(&full_path) {
                        Ok(content) => {
                            let lines: Vec<_> = content.split(|&b| b == b'\n').collect();
                            let line_count = if content.is_empty() { 0 } else { lines.len() };

                            let mut diff = FileDiff::new(&path_str, FileChangeStatus::Untracked);

                            // Create a single graph_op with all new content
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

                            // Create a single graph_op with all new content
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
                    // Use get_file_content_via_overlay so local workspaces see
                    // their parent chain's content via the overlay model.
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

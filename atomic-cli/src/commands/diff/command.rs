use clap_complete::engine::ArgValueCompleter;

use atomic_repository::WorkspaceTxnMode;

use crate::commands::complete::{complete_change_hashes, complete_view_names};
use crate::commands::workspace_txn::{enter_workspace, observe_workspace, remediation_error};

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
    #[arg(short = 'c', long = "change", add = ArgValueCompleter::new(complete_change_hashes))]
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

    /// Show staged changes (baseline → index) in `--git` mode.
    ///
    /// Without `--git` this flag is reserved and has no effect.
    #[arg(long)]
    pub cached: bool,

    /// Git-inspired, versioned staging diff (RFC §9.1/§9.2).
    ///
    /// Compares two distinct layers: with `--cached`, the Git baseline
    /// (HEAD tree) against the index (staged); by default, the index against
    /// the worktree (unstaged). This is a bounded subset; the native
    /// baseline→worktree diff remains the default behavior without `--git`.
    #[arg(long)]
    pub git: bool,

    /// View to compare against.
    #[arg(long, add = ArgValueCompleter::new(complete_view_names))]
    pub view: Option<String>,

    /// Show the active private snapshot or durable remainder.
    #[arg(long, conflicts_with_all = ["change", "view"])]
    pub snapshot: bool,

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
            git: false,
            view: None,
            snapshot: false,
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

    /// Builder: show the active snapshot or remainder.
    pub fn with_snapshot(mut self, snapshot: bool) -> Self {
        self.snapshot = snapshot;
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
            show_line_numbers: false,
            show_path_prefix: true,
            word_diff: self.word_diff,
        }
    }

    fn workspace_mode(&self) -> WorkspaceTxnMode {
        if self.change.is_some() || self.snapshot || self.git {
            WorkspaceTxnMode::Observe
        } else {
            WorkspaceTxnMode::Reconcile
        }
    }

    /// Versioned Git-inspired staging diff (CB-11A).
    ///
    /// Read-only comparison of two adjacent layers:
    /// - `--cached`: Git baseline (HEAD tree) → index (staged),
    /// - default: index → worktree (unstaged).
    fn run_git_diff(&self, repo_root: &std::path::Path) -> CliResult<()> {
        use std::collections::BTreeMap;

        // The --git staging diff is a versioned, bounded subset (RFC §9.1):
        // the version marker makes the contract inspectable in every output
        // format and asserts this is not full Git porcelain compatibility.
        println!("# atomic-diff-git v1 (bounded subset; layers: --cached baseline→index, default index→worktree)");

        let mut repo =
            Repository::open_readonly(repo_root).map_err(|e| CliError::InvalidRepository {
                reason: e.to_string(),
            })?;
        let workspace = match observe_workspace(&mut repo)? {
            Ok(workspace) => workspace,
            Err(remediation) => return Err(remediation_error(remediation)),
        };
        let _ = workspace;

        let git_repo = git2::Repository::open(repo_root).map_err(|error| CliError::GitError {
            message: format!("cannot open Git repository: {error}"),
        })?;
        let policy = crate::commands::git::parallel::conversion_policy(&git_repo)?;
        let filter = atomic_repository::GitAttributesFilter::for_repository(repo_root);

        // Baseline: HEAD tree blobs (read-only).
        let mut baseline: BTreeMap<String, (Vec<u8>, u32)> = BTreeMap::new();
        if let Ok(head) = git_repo.head() {
            if let Ok(commit) = head.peel_to_commit() {
                if let Ok(tree) = commit.tree() {
                    collect_baseline_blobs(&git_repo, &tree, &mut Vec::new(), &mut baseline)?;
                }
            }
        }

        // Index: stage-0 entries with blob content (read-only ODB access).
        let index_state =
            atomic_repository::observe_git_index(repo_root, &policy).map_err(|error| {
                CliError::GitError {
                    message: error.to_string(),
                }
            })?;
        let mut index: BTreeMap<String, (Vec<u8>, u32)> = BTreeMap::new();
        for entry in &index_state.entries {
            if entry.stage != 0 || entry.sparse_directory {
                continue;
            }
            let path = String::from_utf8_lossy(entry.path.as_bytes()).into_owned();
            let Some(oid) = &entry.oid else { continue };
            let gid =
                git2::Oid::from_bytes(oid.as_bytes()).map_err(|error| CliError::GitError {
                    message: format!("cannot parse index object id: {error}"),
                })?;
            let content = git_repo
                .find_blob(gid)
                .map(|blob| blob.content().to_vec())
                .unwrap_or_default();
            index.insert(path, (content, entry.mode));
        }

        // Worktree: cleaned repository bytes for paths of interest.
        let mut worktree_content: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut read_worktree = |path: &str, worktree: &mut BTreeMap<String, Vec<u8>>| {
            if worktree.contains_key(path) {
                return;
            }
            let native = repo_root.join(path);
            let Ok(metadata) = std::fs::symlink_metadata(&native) else {
                return;
            };
            let bytes = if metadata.file_type().is_symlink() {
                std::fs::read_link(&native)
                    .map(|target| target.to_string_lossy().into_owned().into_bytes())
                    .unwrap_or_default()
            } else if metadata.is_file() {
                match std::fs::read(&native) {
                    Ok(bytes) => bytes,
                    Err(_) => return,
                }
            } else {
                return;
            };
            let cleaned = atomic_repository::ContentFilter::clean(
                &filter,
                std::path::Path::new(path),
                &bytes,
            )
            .map(|filtered| filtered.bytes)
            .unwrap_or(bytes);
            worktree.insert(path.to_string(), cleaned);
        };

        let algorithm = self.parse_algorithm()?;
        let mut file_diffs: Vec<FileDiff> = Vec::new();
        let mut stats = DiffStats::new();

        if self.cached {
            // Staged layer: baseline → index.
            let paths: BTreeMap<String, ()> = baseline
                .keys()
                .chain(index.keys())
                .map(|path| (path.clone(), ()))
                .collect();
            for path in paths.keys() {
                if !self.files.is_empty() && !self.files.iter().any(|file| file == path) {
                    continue;
                }
                let old = baseline.get(path).map(|(bytes, _)| bytes.clone());
                let new = index.get(path).map(|(bytes, _)| bytes.clone());
                if old == new {
                    continue;
                }
                let diff = build_layer_file_diff(
                    path,
                    old,
                    new,
                    index.contains_key(path),
                    baseline.contains_key(path),
                    algorithm,
                    self.context,
                )?;
                stats.add_file(diff.stats.clone());
                file_diffs.push(diff);
            }
        } else {
            // Unstaged layer: index → worktree. skip-worktree and
            // assume-unchanged entries are exempt from worktree comparison.
            let flagged: std::collections::BTreeSet<String> = index_state
                .entries
                .iter()
                .filter(|entry| entry.stage == 0 && (entry.skip_worktree || entry.assume_unchanged))
                .map(|entry| String::from_utf8_lossy(entry.path.as_bytes()).into_owned())
                .collect();
            let paths: BTreeMap<String, ()> = index
                .keys()
                .filter(|path| !flagged.contains(*path))
                .map(|path| (path.clone(), ()))
                .collect();
            for path in paths.keys() {
                if !self.files.is_empty() && !self.files.iter().any(|file| file == path) {
                    continue;
                }
                read_worktree(path, &mut worktree_content);
                let old = index.get(path).map(|(bytes, _)| bytes.clone());
                let new = worktree_content.get(path).cloned();
                if old == new {
                    continue;
                }
                let diff = build_layer_file_diff(
                    path,
                    old,
                    new,
                    worktree_content.contains_key(path),
                    index.contains_key(path),
                    algorithm,
                    self.context,
                )?;
                stats.add_file(diff.stats.clone());
                file_diffs.push(diff);
            }
        }

        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        let config = self.get_output_config();
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, &config),
            DiffFormat::Stat => self.print_stat(&stats, &config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, &config),
        }
    }
}

/// Collect HEAD tree blob entries recursively into `path -> (bytes, mode)`.
fn collect_baseline_blobs(
    repository: &git2::Repository,
    tree: &git2::Tree<'_>,
    #[allow(clippy::ptr_arg)] // recursive tree walker shares the Vec
    prefix: &mut Vec<u8>,
    output: &mut std::collections::BTreeMap<String, (Vec<u8>, u32)>,
) -> CliResult<()> {
    for entry in tree.iter() {
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name_bytes());
        if entry.kind() == Some(git2::ObjectType::Tree) {
            let child = repository
                .find_tree(entry.id())
                .map_err(|error| CliError::GitError {
                    message: format!("cannot read subtree {}: {error}", entry.id()),
                })?;
            collect_baseline_blobs(repository, &child, &mut path, output)?;
        } else {
            let content = if entry.kind() == Some(git2::ObjectType::Blob) {
                repository
                    .find_blob(entry.id())
                    .map(|blob| blob.content().to_vec())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            output.insert(
                String::from_utf8_lossy(&path).into_owned(),
                (content, entry.filemode() as u32),
            );
        }
    }
    Ok(())
}

/// Build one file diff between two layer byte-sets.
fn build_layer_file_diff(
    path: &str,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
    new_present: bool,
    old_present: bool,
    algorithm: Algorithm,
    context: usize,
) -> CliResult<FileDiff> {
    let (diff, old_bytes, new_bytes) = match (old_present.then_some(()), new_present.then_some(()))
    {
        (Some(()), Some(())) => {
            let diff = FileDiff::modified(path);
            (diff, old.unwrap_or_default(), new.unwrap_or_default())
        }
        (None, Some(())) => {
            let diff = FileDiff::added(path);
            (diff, Vec::new(), new.unwrap_or_default())
        }
        (Some(()), None) => {
            let diff = FileDiff::deleted(path);
            (diff, old.unwrap_or_default(), Vec::new())
        }
        (None, None) => {
            return Err(CliError::InvalidArgument {
                message: format!("diff for '{path}' has no content on either side"),
            })
        }
    };

    let diff_result = diff_text(&old_bytes, &new_bytes, algorithm);
    let mut diff = diff;
    if !diff_result.is_unchanged() {
        let old_lines: Vec<_> = old_bytes.split(|&b| b == b'\n').collect();
        let new_lines: Vec<_> = new_bytes.split(|&b| b == b'\n').collect();
        let hunks = build_hunks_from_diff(&diff_result, &old_lines, &new_lines, context);
        for hunk in hunks {
            diff.add_hunk(hunk);
        }
    }
    diff.compute_stats();
    Ok(diff)
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

        // Git-inspired, versioned staging diff (observation-only).
        if self.git {
            return self.run_git_diff(&repo_root);
        }

        // Open the repository and retain its stable workspace boundary for all diff work.
        let mode = self.workspace_mode();
        let mut repo = match mode {
            WorkspaceTxnMode::Observe => crate::commands::open_readonly_repository(&repo_root),
            WorkspaceTxnMode::Reconcile => Repository::open_for_workspace_transaction_wait(
                &repo_root,
                std::time::Duration::from_secs(10),
            ),
            WorkspaceTxnMode::Force => unreachable!("diff never forces workspace entry"),
        }
        .map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;
        let workspace = match mode {
            WorkspaceTxnMode::Observe => match observe_workspace(&mut repo)? {
                Ok(workspace) => Some(workspace),
                Err(_) if self.change.is_some() => None,
                Err(remediation) => return Err(remediation_error(remediation)),
            },
            WorkspaceTxnMode::Reconcile => Some(enter_workspace(&mut repo, mode)?),
            WorkspaceTxnMode::Force => unreachable!("diff never forces workspace entry"),
        };
        // Parse algorithm
        let algorithm = self.parse_algorithm()?;

        // Get output configuration
        let config = self.get_output_config();

        // If --change is specified, show the content of that specific change
        if let Some(change_ref) = &self.change {
            return self.show_change_diff(&repo, change_ref, &config);
        }
        if self.snapshot {
            let snapshot = repo
                .snapshot_status(
                    workspace
                        .as_ref()
                        .expect("snapshot diff requires a ready Observe transaction")
                        .working_copy(),
                )
                .map_err(|error| CliError::Internal(error.into()))?;
            let hash = snapshot.snapshot.or(snapshot.remainder).ok_or_else(|| {
                CliError::InvalidArgument {
                    message: "this working copy has no active snapshot or remainder".to_string(),
                }
            })?;
            return self.show_change_diff(&repo, &hash.to_base32(), &config);
        }

        // Get status to find modified files
        let status_options = StatusOptions::default();
        let status = repo
            .status(
                workspace
                    .as_ref()
                    .expect("working-copy diff uses Reconcile")
                    .working_copy(),
                status_options,
            )
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

        let workspace_view = &workspace
            .as_ref()
            .expect("working-copy diff uses Reconcile")
            .view()
            .name;

        // Compute diffs for each file
        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for (path, file_status) in &files_to_diff {
            let path_str = path.display().to_string();
            let _change_status = FileChangeStatus::from(*file_status);

            match file_status {
                FileStatus::Deleted => {
                    // For deleted files, retrieve the old content from the graph
                    let old_content = match repo.get_file_content_on_view(path, workspace_view) {
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
                    let old_content = match repo.get_file_content_on_view(path, workspace_view) {
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

#[cfg(test)]
mod workspace_mode_tests {
    use super::*;

    #[test]
    fn change_diff_observes_workspace() {
        assert_eq!(Diff::new().workspace_mode(), WorkspaceTxnMode::Reconcile);
        assert_eq!(
            Diff::new().with_change("change").workspace_mode(),
            WorkspaceTxnMode::Observe
        );
    }

    #[test]
    fn snapshot_diff_observes_workspace() {
        assert_eq!(
            Diff::new().with_snapshot(true).workspace_mode(),
            WorkspaceTxnMode::Observe
        );
    }
}

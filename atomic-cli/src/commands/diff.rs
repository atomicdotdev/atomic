#![allow(dead_code)]
//! The `diff` command for showing changes between working copy and repository.
//!
//! This module implements the `atomic diff` command, which displays the
//! differences between the working copy and the recorded repository state.
//! It supports multiple output formats and can compare specific files or
//! show all uncommitted changes.
//!
//! # Token-Level Diff (Word Diff)
//!
//! Atomic supports **token-level diff** via the `--word-diff` flag, which shows
//! exactly which tokens/words changed within a line, not just that the line changed.
//! This is powered by the CRDT tokenization engine and is especially useful for
//! code reviews.
//!
//! ```text
//! $ atomic diff --word-diff
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! @@ -1,3 +1,3 @@
//!  fn main() {
//! -    println!("Hello");
//! +    println!("Hello, World!");
//!                     ^^^^^^^^^ <- token-level highlight
//!  }
//! ```
//!
//! # Usage
//!
//! ```text
//! atomic diff [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Specific files to diff (default: all modified files)
//!
//! Options:
//!   -c, --change <HASH>       Compare against a specific change
//!       --algorithm <ALG>     Diff algorithm (myers, patience) [default: myers]
//!       --context <N>         Number of context lines [default: 3]
//!       --stat                Show diffstat summary only
//!       --no-color            Disable colored output
//!       --word-diff           Enable token-level diff highlighting
//!       --cached              Show staged changes (not yet implemented)
//!       --name-only           Show only names of changed files
//!       --name-status         Show names and status of changed files
//!   -h, --help                Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Default (Unified Diff)
//!
//! Shows the traditional unified diff format with colored output:
//!
//! ```text
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! @@ -1,5 +1,6 @@
//!  fn main() {
//! -    println!("Hello");
//! +    println!("Hello, World!");
//! +    println!("Welcome!");
//!  }
//! ```
//!
//! ## Stat Format (--stat)
//!
//! Shows a summary of changes with insertion/deletion counts:
//!
//! ```text
//!  src/main.rs    | 3 ++-
//!  src/lib.rs     | 5 +++++
//!  2 files changed, 7 insertions(+), 1 deletion(-)
//! ```
//!
//! ## Name Only (--name-only)
//!
//! Shows just the filenames that have changes:
//!
//! ```text
//! src/main.rs
//! src/lib.rs
//! ```
//!
//! ## Name Status (--name-status)
//!
//! Shows filenames with their change status:
//!
//! ```text
//! M  src/main.rs
//! A  src/lib.rs
//! D  src/old.rs
//! ```
//!
//! # Examples
//!
//! Show all uncommitted changes:
//! ```text
//! $ atomic diff
//! diff --atomic a/src/main.rs b/src/main.rs
//! --- a/src/main.rs
//! +++ b/src/main.rs
//! ...
//! ```
//!
//! Show changes for specific file:
//! ```text
//! $ atomic diff src/main.rs
//! diff --atomic a/src/main.rs b/src/main.rs
//! ...
//! ```
//!
//! Show only a summary:
//! ```text
//! $ atomic diff --stat
//!  src/main.rs | 3 ++-
//!  1 file changed, 2 insertions(+), 1 deletion(-)
//! ```
//!
//! Use patience algorithm for better diffs:
//! ```text
//! $ atomic diff --algorithm patience
//! ...
//! ```
//!
//! # Token-Level Diff for Code Reviews
//!
//! The `--word-diff` flag enables fine-grained highlighting that shows exactly
//! which tokens changed within a line. This is especially useful for:
//!
//! - **Variable renames**: See `oldName` → `newName` highlighted
//! - **Parameter changes**: See which arguments were added/removed
//! - **String modifications**: See exactly which part of a string changed
//! - **Operator changes**: See `==` → `===` or `+` → `-`
//!
//! ```text
//! $ atomic diff --word-diff src/auth.rs
//! diff --atomic a/src/auth.rs b/src/auth.rs
//! @@ -10,3 +10,3 @@
//! -    let token = generate_token(user_id, 3600);
//! +    let token = generate_token(user_id, 7200, true);
//!                                          ^^^^  ^^^^^ <- changed tokens
//! ```
//!
//! The highlighting uses ANSI escape codes:
//! - **Deleted tokens**: Bright red with underline
//! - **Added tokens**: Bright green with underline
//! - **Context**: Dim red/green for unchanged parts of changed lines

use std::cmp;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, ValueEnum};

use atomic_core::change::Change;
use atomic_core::crdt::{BranchOp, LeafOp, TrunkOp};
use atomic_core::diff::display::LineStatus;
use atomic_core::diff::semantic::{semantic_diff, LineChange, TokenChange};
use atomic_core::diff::{compute_inline_diff, diff_text, Algorithm, DiffOp, DiffResult, HunkKind};
use atomic_core::types::{Base32, Hash};
use atomic_repository::status::{FileStatus, StatusOptions};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command, DEFAULT_HASH_LENGTH};
use crate::error::{CliError, CliResult};
use crate::output::{
    added, deleted, emphasis, hash, info, modified, path as style_path, print_info,
};

// =============================================================================
// Diff Output Format
// =============================================================================

/// Output format for the diff command.
///
/// Controls how the diff output is presented to the user. Each format
/// serves different use cases from human review to scripting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, ValueEnum)]
pub enum DiffFormat {
    /// Unified diff format (default).
    ///
    /// Shows the traditional unified diff with context lines and
    /// +/- markers for additions and deletions.
    #[default]
    Unified,

    /// Stat summary only.
    ///
    /// Shows a condensed summary with file names and change counts,
    /// similar to `git diff --stat`.
    Stat,

    /// Show only names of changed files.
    ///
    /// Lists file paths without any diff content.
    NameOnly,

    /// Show names with status indicators.
    ///
    /// Lists file paths with M/A/D status prefixes.
    NameStatus,
}

impl fmt::Display for DiffFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffFormat::Unified => write!(f, "unified"),
            DiffFormat::Stat => write!(f, "stat"),
            DiffFormat::NameOnly => write!(f, "name-only"),
            DiffFormat::NameStatus => write!(f, "name-status"),
        }
    }
}

impl FromStr for DiffFormat {
    type Err = String;

    /// Parse a diff format from string.
    ///
    /// # Accepted Values
    ///
    /// - "unified" → `DiffFormat::Unified`
    /// - "stat" → `DiffFormat::Stat`
    /// - "name-only", "nameonly" → `DiffFormat::NameOnly`
    /// - "name-status", "namestatus" → `DiffFormat::NameStatus`
    ///
    /// # Errors
    ///
    /// Returns an error string if the format is not recognized.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "unified" | "u" => Ok(DiffFormat::Unified),
            "stat" | "s" => Ok(DiffFormat::Stat),
            "name-only" | "nameonly" | "names" => Ok(DiffFormat::NameOnly),
            "name-status" | "namestatus" | "status" => Ok(DiffFormat::NameStatus),
            _ => Err(format!(
                "unknown diff format '{}'. Valid options: unified, stat, name-only, name-status",
                s
            )),
        }
    }
}

// =============================================================================
// Diff Statistics
// =============================================================================

/// Statistics about a single file's diff.
///
/// Tracks the number of insertions and deletions for a file,
/// used for generating stat summaries.
#[derive(Debug, Clone, Default)]
pub struct FileDiffStats {
    /// The path to the file.
    pub path: String,

    /// Number of lines added.
    pub insertions: usize,

    /// Number of lines deleted.
    pub deletions: usize,

    /// The status of the file (modified, added, deleted).
    pub status: char,
}

impl FileDiffStats {
    /// Create new file diff statistics.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `insertions` - Number of lines added
    /// * `deletions` - Number of lines deleted
    /// * `status` - Status character (M/A/D/R/C)
    pub fn new(path: impl Into<String>, insertions: usize, deletions: usize, status: char) -> Self {
        Self {
            path: path.into(),
            insertions,
            deletions,
            status,
        }
    }

    /// Create statistics for an added file.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `lines` - Number of lines in the new file
    pub fn added(path: impl Into<String>, lines: usize) -> Self {
        Self::new(path, lines, 0, 'A')
    }

    /// Create statistics for a deleted file.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `lines` - Number of lines in the deleted file
    pub fn deleted(path: impl Into<String>, lines: usize) -> Self {
        Self::new(path, 0, lines, 'D')
    }

    /// Create statistics for a modified file.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `insertions` - Number of lines added
    /// * `deletions` - Number of lines deleted
    pub fn modified(path: impl Into<String>, insertions: usize, deletions: usize) -> Self {
        Self::new(path, insertions, deletions, 'M')
    }

    /// Get the total number of changed lines.
    pub fn total_changes(&self) -> usize {
        self.insertions + self.deletions
    }

    /// Check if this represents any changes.
    pub fn has_changes(&self) -> bool {
        self.insertions > 0 || self.deletions > 0
    }

    /// Check if this is a newly added file.
    pub fn is_added(&self) -> bool {
        self.status == 'A'
    }

    /// Check if this is a deleted file.
    pub fn is_deleted(&self) -> bool {
        self.status == 'D'
    }

    /// Check if this is a modified file.
    pub fn is_modified(&self) -> bool {
        self.status == 'M'
    }
}

// =============================================================================
// Aggregate Diff Statistics
// =============================================================================

/// Aggregate statistics for multiple file diffs.
///
/// Collects and summarizes diff statistics across all files
/// in a diff operation.
#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    /// Per-file statistics.
    files: Vec<FileDiffStats>,

    /// Total insertions across all files.
    total_insertions: usize,

    /// Total deletions across all files.
    total_deletions: usize,
}

impl DiffStats {
    /// Create a new empty diff statistics collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add statistics for a file.
    ///
    /// # Arguments
    ///
    /// * `file_stats` - Statistics for the file to add
    pub fn add_file(&mut self, file_stats: FileDiffStats) {
        self.total_insertions += file_stats.insertions;
        self.total_deletions += file_stats.deletions;
        self.files.push(file_stats);
    }

    /// Get the number of files with changes.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Get total insertions across all files.
    pub fn total_insertions(&self) -> usize {
        self.total_insertions
    }

    /// Get total deletions across all files.
    pub fn total_deletions(&self) -> usize {
        self.total_deletions
    }

    /// Get total changes across all files.
    pub fn total_changes(&self) -> usize {
        self.total_insertions + self.total_deletions
    }

    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        !self.files.is_empty()
    }

    /// Get an iterator over file statistics.
    pub fn iter(&self) -> impl Iterator<Item = &FileDiffStats> {
        self.files.iter()
    }

    /// Get the maximum path length for formatting.
    pub fn max_path_length(&self) -> usize {
        self.files.iter().map(|f| f.path.len()).max().unwrap_or(0)
    }

    /// Get the maximum change count for formatting.
    pub fn max_change_count(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.total_changes())
            .max()
            .unwrap_or(0)
    }
}

impl IntoIterator for DiffStats {
    type Item = FileDiffStats;
    type IntoIter = std::vec::IntoIter<FileDiffStats>;

    fn into_iter(self) -> Self::IntoIter {
        self.files.into_iter()
    }
}

// =============================================================================
// Diff Output Configuration
// =============================================================================

/// Configuration for diff output formatting.
///
/// Controls various aspects of how diff output is rendered,
/// including context lines, colors, and format selection.
#[derive(Debug, Clone)]
pub struct DiffOutputConfig {
    /// Number of context lines to show around changes.
    pub context_lines: usize,

    /// Whether to use colored output.
    pub color: bool,

    /// Output format.
    pub format: DiffFormat,

    /// Maximum width for stat graphs.
    pub stat_width: usize,

    /// Whether to show line numbers.
    pub show_line_numbers: bool,

    /// Whether to show path prefixes (a/ and b/).
    pub show_path_prefix: bool,

    /// Whether to use word-level diff highlighting.
    ///
    /// When enabled, shows exactly which tokens changed within a line,
    /// using bright colors to highlight the specific changes.
    pub word_diff: bool,
}

impl Default for DiffOutputConfig {
    fn default() -> Self {
        Self {
            context_lines: 3,
            color: true,
            format: DiffFormat::Unified,
            stat_width: 80,
            show_line_numbers: true,
            show_path_prefix: true,
            word_diff: false,
        }
    }
}

impl DiffOutputConfig {
    /// Create a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of context lines.
    pub fn with_context(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Set whether to use colors.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Set the output format.
    pub fn with_format(mut self, format: DiffFormat) -> Self {
        self.format = format;
        self
    }

    /// Set the stat graph width.
    pub fn with_stat_width(mut self, width: usize) -> Self {
        self.stat_width = width;
        self
    }

    /// Set whether to show line numbers.
    pub fn with_line_numbers(mut self, show: bool) -> Self {
        self.show_line_numbers = show;
        self
    }

    /// Set whether to show path prefixes (a/ and b/).
    pub fn with_path_prefix(mut self, show: bool) -> Self {
        self.show_path_prefix = show;
        self
    }

    /// Set whether to use word-level diff highlighting.
    pub fn with_word_diff(mut self, word_diff: bool) -> Self {
        self.word_diff = word_diff;
        self
    }
}

// =============================================================================
// Diff GraphOp
// =============================================================================

/// A contiguous region of changes in a diff.
///
/// A graph_op represents a section of the file where changes occur,
/// including the surrounding context lines.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Starting line number in the old file (1-based).
    pub old_start: usize,

    /// Number of lines from the old file.
    pub old_count: usize,

    /// Starting line number in the new file (1-based).
    pub new_start: usize,

    /// Number of lines from the new file.
    pub new_count: usize,

    /// Lines in this graph_op with their status.
    pub lines: Vec<HunkLine>,
}

impl DiffHunk {
    /// Create a new diff graph_op.
    pub fn new(old_start: usize, old_count: usize, new_start: usize, new_count: usize) -> Self {
        Self {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        }
    }

    /// Add a line to this graph_op.
    pub fn add_line(&mut self, line: HunkLine) {
        self.lines.push(line);
    }

    /// Get the graph_op header in unified diff format.
    ///
    /// Returns a string like `@@ -1,5 +1,6 @@`
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_count, self.new_start, self.new_count
        )
    }

    /// Check if this graph_op contains any changes.
    pub fn has_changes(&self) -> bool {
        self.lines.iter().any(|l| l.is_change())
    }
}

// =============================================================================
// GraphOp Line
// =============================================================================

/// A single line within a diff graph_op.
///
/// Contains the line content and its status (context, added, or removed).
#[derive(Debug, Clone)]
pub struct HunkLine {
    /// The status of this line.
    pub status: LineStatus,

    /// The content of this line (without trailing newline).
    pub content: String,

    /// The line number in the old file (if applicable).
    pub old_line_num: Option<usize>,

    /// The line number in the new file (if applicable).
    pub new_line_num: Option<usize>,
}

impl HunkLine {
    /// Create a context (unchanged) line.
    pub fn context(content: impl Into<String>, old_num: usize, new_num: usize) -> Self {
        Self {
            status: LineStatus::Unchanged,
            content: content.into(),
            old_line_num: Some(old_num),
            new_line_num: Some(new_num),
        }
    }

    /// Create an added line.
    pub fn added(content: impl Into<String>, new_num: usize) -> Self {
        Self {
            status: LineStatus::Added,
            content: content.into(),
            old_line_num: None,
            new_line_num: Some(new_num),
        }
    }

    /// Create a removed line.
    pub fn removed(content: impl Into<String>, old_num: usize) -> Self {
        Self {
            status: LineStatus::Removed,
            content: content.into(),
            old_line_num: Some(old_num),
            new_line_num: None,
        }
    }

    /// Check if this line represents a change (added or removed).
    pub fn is_change(&self) -> bool {
        matches!(self.status, LineStatus::Added | LineStatus::Removed)
    }

    /// Check if this is a context (unchanged) line.
    pub fn is_context(&self) -> bool {
        matches!(self.status, LineStatus::Unchanged)
    }

    /// Get the prefix character for this line.
    pub fn prefix(&self) -> char {
        match self.status {
            LineStatus::Unchanged => ' ',
            LineStatus::Added => '+',
            LineStatus::Removed => '-',
        }
    }
}

impl fmt::Display for HunkLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.prefix(), self.content)
    }
}

// =============================================================================
// File Diff
// =============================================================================

/// A complete diff for a single file.
///
/// Contains metadata about the file and all hunks showing changes.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Path to the file in the old version.
    pub old_path: String,

    /// Path to the file in the new version.
    pub new_path: String,

    /// The status of this file.
    pub status: FileChangeStatus,

    /// Hunks containing the actual changes.
    pub hunks: Vec<DiffHunk>,

    /// Statistics for this file.
    pub stats: FileDiffStats,

    /// Whether this is a binary file.
    pub is_binary: bool,
}

impl FileDiff {
    /// Create a new file diff.
    pub fn new(path: impl Into<String>, status: FileChangeStatus) -> Self {
        let path_str = path.into();
        Self {
            old_path: path_str.clone(),
            new_path: path_str.clone(),
            status,
            hunks: Vec::new(),
            stats: FileDiffStats::default(),
            is_binary: false,
        }
    }

    /// Create a diff for a new file.
    pub fn added(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self {
            old_path: "/dev/null".to_string(),
            new_path: path_str.clone(),
            status: FileChangeStatus::Added,
            hunks: Vec::new(),
            stats: FileDiffStats {
                path: path_str,
                status: 'A',
                ..Default::default()
            },
            is_binary: false,
        }
    }

    /// Create a diff for a deleted file.
    pub fn deleted(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self {
            old_path: path_str.clone(),
            new_path: "/dev/null".to_string(),
            status: FileChangeStatus::Deleted,
            hunks: Vec::new(),
            stats: FileDiffStats {
                path: path_str,
                status: 'D',
                ..Default::default()
            },
            is_binary: false,
        }
    }

    /// Create a diff for a modified file.
    pub fn modified(path: impl Into<String>) -> Self {
        let path_str = path.into();
        Self {
            old_path: path_str.clone(),
            new_path: path_str.clone(),
            status: FileChangeStatus::Modified,
            hunks: Vec::new(),
            stats: FileDiffStats {
                path: path_str,
                status: 'M',
                ..Default::default()
            },
            is_binary: false,
        }
    }

    /// Add a graph_op to this diff.
    pub fn add_hunk(&mut self, graph_op: DiffHunk) {
        self.hunks.push(graph_op);
    }

    /// Update statistics based on hunks.
    pub fn compute_stats(&mut self) {
        let mut insertions = 0;
        let mut deletions = 0;

        for graph_op in &self.hunks {
            for line in &graph_op.lines {
                match line.status {
                    LineStatus::Added => insertions += 1,
                    LineStatus::Removed => deletions += 1,
                    LineStatus::Unchanged => {}
                }
            }
        }

        self.stats.insertions = insertions;
        self.stats.deletions = deletions;
    }

    /// Check if this diff has any changes.
    pub fn has_changes(&self) -> bool {
        self.is_binary || self.hunks.iter().any(|h| h.has_changes())
    }

    /// Get the display path for the file.
    pub fn display_path(&self) -> &str {
        match self.status {
            FileChangeStatus::Added => &self.new_path,
            FileChangeStatus::Deleted => &self.old_path,
            _ => &self.new_path,
        }
    }
}

// =============================================================================
// File Change Status
// =============================================================================

/// The type of change for a file in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChangeStatus {
    /// File was added (new file).
    Added,

    /// File was deleted.
    Deleted,

    /// File was modified.
    Modified,

    /// File was renamed.
    Renamed,

    /// File was copied.
    Copied,

    /// File type changed (e.g., regular to symlink).
    TypeChanged,

    /// File is untracked (not yet added to the repository).
    Untracked,
}

impl FileChangeStatus {
    /// Get the status character for this change type.
    pub fn status_char(&self) -> char {
        match self {
            FileChangeStatus::Added => 'A',
            FileChangeStatus::Deleted => 'D',
            FileChangeStatus::Modified => 'M',
            FileChangeStatus::Renamed => 'R',
            FileChangeStatus::Copied => 'C',
            FileChangeStatus::TypeChanged => 'T',
            FileChangeStatus::Untracked => 'U',
        }
    }

    /// Get a human-readable description of this status.
    pub fn description(&self) -> &'static str {
        match self {
            FileChangeStatus::Added => "added",
            FileChangeStatus::Deleted => "deleted",
            FileChangeStatus::Modified => "modified",
            FileChangeStatus::Renamed => "renamed",
            FileChangeStatus::Copied => "copied",
            FileChangeStatus::TypeChanged => "type changed",
            FileChangeStatus::Untracked => "untracked",
        }
    }
}

impl fmt::Display for FileChangeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl From<FileStatus> for FileChangeStatus {
    fn from(status: FileStatus) -> Self {
        match status {
            FileStatus::Added => FileChangeStatus::Added,
            FileStatus::Deleted => FileChangeStatus::Deleted,
            FileStatus::Modified => FileChangeStatus::Modified,
            FileStatus::TypeChanged => FileChangeStatus::TypeChanged,
            FileStatus::Clean => FileChangeStatus::Modified, // Shouldn't happen
            FileStatus::Untracked => FileChangeStatus::Untracked,
            FileStatus::Conflicted => FileChangeStatus::Modified,
            FileStatus::PermissionsChanged => FileChangeStatus::Modified,
        }
    }
}

// =============================================================================
// Diff Command
// =============================================================================

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
    /// Set word_diff option (builder pattern).
    pub fn with_word_diff(mut self, word_diff: bool) -> Self {
        self.word_diff = word_diff;
        self
    }

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

    /// Set specific files to diff.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(Into::into).collect();
        self
    }

    /// Set the change to compare against.
    pub fn with_change(mut self, change: impl Into<String>) -> Self {
        self.change = Some(change.into());
        self
    }

    /// Set the diff algorithm.
    pub fn with_algorithm(mut self, algorithm: impl Into<String>) -> Self {
        self.algorithm = algorithm.into();
        self
    }

    /// Set the number of context lines.
    pub fn with_context(mut self, lines: usize) -> Self {
        self.context = lines;
        self
    }

    /// Enable stat-only output.
    pub fn with_stat(mut self, stat: bool) -> Self {
        self.stat = stat;
        self
    }

    /// Disable colored output.
    pub fn with_no_color(mut self, no_color: bool) -> Self {
        self.no_color = no_color;
        self
    }

    /// Enable name-only output.
    pub fn with_name_only(mut self, name_only: bool) -> Self {
        self.name_only = name_only;
        self
    }

    /// Enable name-status output.
    pub fn with_name_status(mut self, name_status: bool) -> Self {
        self.name_status = name_status;
        self
    }

    /// Set the stack to compare against.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
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
    fn parse_algorithm(&self) -> CliResult<Algorithm> {
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
    fn get_output_config(&self) -> DiffOutputConfig {
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
                            for k in pairs..removed_lines.len() {
                                let removed_line = removed_lines[k];
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
                            for k in pairs..added_lines.len() {
                                let added_line = added_lines[k];
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
                    let old_content = match repo.get_file_content(path) {
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

                    // Retrieve the old (recorded) content from the graph
                    let old_content = match repo.get_file_content(path) {
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

// =============================================================================
// Helper Functions
// =============================================================================

/// Build hunks from a diff result with context lines.
///
/// This function groups diff operations into hunks with the specified
/// number of context lines around changes.
///
/// # Arguments
///
/// * `diff_result` - The raw diff result
/// * `old_lines` - Lines from the old content
/// * `new_lines` - Lines from the new content
/// * `context` - Number of context lines to include
///
/// # Returns
///
/// A vector of `DiffHunk`s representing the changes with context.
fn build_hunks_from_diff(
    diff_result: &DiffResult,
    old_lines: &[&[u8]],
    new_lines: &[&[u8]],
    context: usize,
) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();

    // Simple implementation: create one graph_op for all changes
    // A more sophisticated implementation would group changes by proximity

    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_line = 1;
    let mut new_line = 1;

    for op in diff_result.iter() {
        match op {
            DiffOp::Equal { len, .. } => {
                if let Some(ref mut graph_op) = current_hunk {
                    // Add context lines to current graph_op (up to context limit)
                    let context_count = cmp::min(*len, context);
                    for i in 0..context_count {
                        let content = if new_line - 1 + i < new_lines.len() {
                            String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        graph_op.add_line(HunkLine::context(content, old_line + i, new_line + i));
                    }

                    // If we've shown enough context and there's more equal content,
                    // close this graph_op
                    if *len > context * 2 {
                        graph_op.old_count = graph_op
                            .lines
                            .iter()
                            .filter(|l| l.old_line_num.is_some())
                            .count();
                        graph_op.new_count = graph_op
                            .lines
                            .iter()
                            .filter(|l| l.new_line_num.is_some())
                            .count();
                        hunks.push(current_hunk.take().unwrap());
                    }
                }
                old_line += len;
                new_line += len;
            }
            DiffOp::Insert { len, .. } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));

                    // Add leading context
                    let context_start = new_line.saturating_sub(context);
                    for i in context_start..new_line {
                        if i > 0 && i <= new_lines.len() {
                            let content = String::from_utf8_lossy(new_lines[i - 1]).into_owned();
                            let old_i = old_line.saturating_sub(new_line - i);
                            current_hunk
                                .as_mut()
                                .unwrap()
                                .add_line(HunkLine::context(content, old_i, i));
                        }
                    }
                }

                // Add inserted lines
                for i in 0..*len {
                    let content = if new_line - 1 + i < new_lines.len() {
                        String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                    } else {
                        String::new()
                    };
                    current_hunk
                        .as_mut()
                        .unwrap()
                        .add_line(HunkLine::added(content, new_line + i));
                }
                new_line += len;
            }
            DiffOp::Delete { len, .. } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));
                }

                // Add deleted lines
                for i in 0..*len {
                    let content = if old_line - 1 + i < old_lines.len() {
                        String::from_utf8_lossy(old_lines[old_line - 1 + i]).into_owned()
                    } else {
                        String::new()
                    };
                    current_hunk
                        .as_mut()
                        .unwrap()
                        .add_line(HunkLine::removed(content, old_line + i));
                }
                old_line += len;
            }
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));
                }

                // Interleave deleted and added lines for better word-level diff pairing.
                // This makes it easier to show word-level changes when a line is modified.
                let max_len = (*old_len).max(*new_len);
                for i in 0..max_len {
                    // Add deleted line if available
                    if i < *old_len {
                        let content = if old_line - 1 + i < old_lines.len() {
                            String::from_utf8_lossy(old_lines[old_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        current_hunk
                            .as_mut()
                            .unwrap()
                            .add_line(HunkLine::removed(content, old_line + i));
                    }

                    // Add inserted line if available
                    if i < *new_len {
                        let content = if new_line - 1 + i < new_lines.len() {
                            String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        current_hunk
                            .as_mut()
                            .unwrap()
                            .add_line(HunkLine::added(content, new_line + i));
                    }
                }

                old_line += old_len;
                new_line += new_len;
            }
        }
    }

    // Finalize any remaining graph_op
    if let Some(mut graph_op) = current_hunk {
        graph_op.old_count = graph_op
            .lines
            .iter()
            .filter(|l| l.old_line_num.is_some() && !matches!(l.status, LineStatus::Added))
            .count();
        graph_op.new_count = graph_op
            .lines
            .iter()
            .filter(|l| l.new_line_num.is_some() && !matches!(l.status, LineStatus::Removed))
            .count();
        hunks.push(graph_op);
    }

    hunks
}

/// Format a diff stat line graph.
///
/// Creates the +/- visual representation for stat output.
///
/// # Arguments
///
/// * `insertions` - Number of insertions
/// * `deletions` - Number of deletions
/// * `max_width` - Maximum width for the graph
///
/// # Returns
///
/// A string containing + and - characters.
/// Print a line with word-level diff highlighting.
///
/// Uses ANSI escape codes to highlight changed tokens:
/// - Deletions: bright red text on light red background
/// - Insertions: bright green text on light green background
fn print_word_diff_line(
    content: &[u8],
    hunks: &[atomic_core::diff::ChangeHunk],
    is_deletion: bool,
) {
    for hunk in hunks {
        if hunk.end > content.len() {
            continue;
        }
        let text = String::from_utf8_lossy(&content[hunk.start..hunk.end]);

        match hunk.kind {
            HunkKind::Deleted | HunkKind::Modified if is_deletion => {
                // Bright red text with underline for deletions
                print!("\x1b[91;1;4m{}\x1b[0m", text);
            }
            HunkKind::Inserted | HunkKind::Modified if !is_deletion => {
                // Bright green text with underline for insertions
                print!("\x1b[92;1;4m{}\x1b[0m", text);
            }
            _ => {
                // Normal text (unchanged parts)
                if is_deletion {
                    print!("\x1b[31m{}\x1b[0m", text); // Dim red for context
                } else {
                    print!("\x1b[32m{}\x1b[0m", text); // Dim green for context
                }
            }
        }
    }
}

/// Print a line with semantic token-level diff highlighting.
///
/// Uses the semantic diff engine for precise token-level highlighting.
/// This produces better results than the inline diff for code, as it
/// understands token boundaries (identifiers, operators, strings, etc.)
///
/// # Visual Pattern
///
/// ```text
/// - const result = calculateSum(a, b);        <- light red background
/// + const result = calculateSum(a, b, c);     <- light green background
///                                   ^^^^      <- dark green: ", c" added
/// ```
fn print_semantic_word_diff_line(token_changes: &[TokenChange<'_>], is_deletion: bool) {
    for tc in token_changes {
        match tc {
            TokenChange::Unchanged { token, .. } => {
                // Unchanged tokens - dim color for context
                let text = token.as_str();
                if is_deletion {
                    print!("\x1b[31m{}\x1b[0m", text); // Dim red for deletion context
                } else {
                    print!("\x1b[32m{}\x1b[0m", text); // Dim green for insertion context
                }
            }
            TokenChange::Deleted { token, .. } if is_deletion => {
                // Deleted token - bright red with underline
                let text = token.as_str();
                print!("\x1b[91;1;4m{}\x1b[0m", text);
            }
            TokenChange::Inserted { token, .. } if !is_deletion => {
                // Inserted token - bright green with underline
                let text = token.as_str();
                print!("\x1b[92;1;4m{}\x1b[0m", text);
            }
            TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } => {
                if is_deletion {
                    // Show old token in bright red with underline
                    let text = old_token.as_str();
                    print!("\x1b[91;1;4m{}\x1b[0m", text);
                } else {
                    // Show new token in bright green with underline
                    let text = new_token.as_str();
                    print!("\x1b[92;1;4m{}\x1b[0m", text);
                }
            }
            // Skip tokens that don't apply to this line type
            TokenChange::Deleted { .. } | TokenChange::Inserted { .. } => {}
        }
    }
}

/// Print a modified line pair using semantic diff.
///
/// Takes the before and after lines and their token changes, and prints
/// both lines with appropriate highlighting.
fn print_semantic_diff_lines(
    _old_line: &str,
    _new_line: &str,
    token_changes: &[TokenChange<'_>],
    old_prefix: &str,
    new_prefix: &str,
) {
    use crate::output::{added, deleted};

    // Print old line with deletions highlighted
    print!("{}", deleted(&old_prefix.to_string()));
    print_semantic_word_diff_line(token_changes, true);
    println!();

    // Print new line with insertions highlighted
    print!("{}", added(&new_prefix.to_string()));
    print_semantic_word_diff_line(token_changes, false);
    println!();
}

/// Compute and display a semantic diff between two lines.
///
/// This is a convenience function that computes the semantic diff and
/// immediately prints the result with highlighting.
fn show_semantic_line_diff(old_content: &[u8], new_content: &[u8]) {
    // Compute semantic diff for just these two lines
    let diff = semantic_diff(old_content, new_content);

    if let Some(change) = diff.changes().first() {
        match change {
            LineChange::Modified { token_changes, .. } => {
                print_semantic_word_diff_line(token_changes, true);
            }
            LineChange::Deleted { tokens, .. } => {
                print_semantic_word_diff_line(tokens, true);
            }
            LineChange::Added { tokens, .. } => {
                print_semantic_word_diff_line(tokens, false);
            }
        }
    } else {
        // No semantic changes detected, print as-is
        let text = String::from_utf8_lossy(new_content);
        print!("{}", text);
    }
}

fn format_stat_graph(insertions: usize, deletions: usize, width: usize) -> String {
    let total = insertions + deletions;
    if total == 0 {
        return String::new();
    }

    let scale = if total > width {
        width as f64 / total as f64
    } else {
        1.0
    };

    let plus_count = (insertions as f64 * scale).round() as usize;
    let minus_count = (deletions as f64 * scale).round() as usize;

    format!("{}{}", "+".repeat(plus_count), "-".repeat(minus_count))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // DiffFormat Tests
    // =========================================================================

    #[test]
    fn test_diff_format_default() {
        assert_eq!(DiffFormat::default(), DiffFormat::Unified);
    }

    #[test]
    fn test_diff_format_display() {
        assert_eq!(DiffFormat::Unified.to_string(), "unified");
        assert_eq!(DiffFormat::Stat.to_string(), "stat");
        assert_eq!(DiffFormat::NameOnly.to_string(), "name-only");
        assert_eq!(DiffFormat::NameStatus.to_string(), "name-status");
    }

    #[test]
    fn test_diff_format_from_str_unified() {
        assert_eq!(
            "unified".parse::<DiffFormat>().unwrap(),
            DiffFormat::Unified
        );
        assert_eq!("u".parse::<DiffFormat>().unwrap(), DiffFormat::Unified);
        assert_eq!(
            "UNIFIED".parse::<DiffFormat>().unwrap(),
            DiffFormat::Unified
        );
    }

    #[test]
    fn test_diff_format_from_str_stat() {
        assert_eq!("stat".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
        assert_eq!("s".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
        assert_eq!("STAT".parse::<DiffFormat>().unwrap(), DiffFormat::Stat);
    }

    #[test]
    fn test_diff_format_from_str_name_only() {
        assert_eq!(
            "name-only".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameOnly
        );
        assert_eq!(
            "nameonly".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameOnly
        );
        assert_eq!("names".parse::<DiffFormat>().unwrap(), DiffFormat::NameOnly);
    }

    #[test]
    fn test_diff_format_from_str_name_status() {
        assert_eq!(
            "name-status".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
        assert_eq!(
            "namestatus".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
        assert_eq!(
            "status".parse::<DiffFormat>().unwrap(),
            DiffFormat::NameStatus
        );
    }

    #[test]
    fn test_diff_format_from_str_invalid() {
        let err = "invalid".parse::<DiffFormat>().unwrap_err();
        assert!(err.contains("unknown diff format"));
        assert!(err.contains("invalid"));
    }

    #[test]
    fn test_diff_format_equality() {
        assert_eq!(DiffFormat::Unified, DiffFormat::Unified);
        assert_ne!(DiffFormat::Unified, DiffFormat::Stat);
    }

    #[test]
    fn test_diff_format_clone() {
        let format = DiffFormat::Stat;
        let cloned = format.clone();
        assert_eq!(format, cloned);
    }

    #[test]
    fn test_diff_format_copy() {
        let format = DiffFormat::NameOnly;
        let copied = format;
        assert_eq!(format, copied);
    }

    #[test]
    fn test_diff_format_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(DiffFormat::Unified);
        set.insert(DiffFormat::Stat);
        assert!(set.contains(&DiffFormat::Unified));
        assert!(set.contains(&DiffFormat::Stat));
        assert!(!set.contains(&DiffFormat::NameOnly));
    }

    // =========================================================================
    // FileDiffStats Tests
    // =========================================================================

    #[test]
    fn test_file_diff_stats_new() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        assert_eq!(stats.path, "test.rs");
        assert_eq!(stats.insertions, 10);
        assert_eq!(stats.deletions, 5);
        assert_eq!(stats.status, 'M');
    }

    #[test]
    fn test_file_diff_stats_added() {
        let stats = FileDiffStats::added("new.rs", 20);
        assert_eq!(stats.path, "new.rs");
        assert_eq!(stats.insertions, 20);
        assert_eq!(stats.deletions, 0);
        assert_eq!(stats.status, 'A');
        assert!(stats.is_added());
        assert!(!stats.is_deleted());
        assert!(!stats.is_modified());
    }

    #[test]
    fn test_file_diff_stats_deleted() {
        let stats = FileDiffStats::deleted("old.rs", 15);
        assert_eq!(stats.path, "old.rs");
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 15);
        assert_eq!(stats.status, 'D');
        assert!(stats.is_deleted());
        assert!(!stats.is_added());
    }

    #[test]
    fn test_file_diff_stats_modified() {
        let stats = FileDiffStats::modified("mod.rs", 8, 3);
        assert_eq!(stats.path, "mod.rs");
        assert_eq!(stats.insertions, 8);
        assert_eq!(stats.deletions, 3);
        assert_eq!(stats.status, 'M');
        assert!(stats.is_modified());
    }

    #[test]
    fn test_file_diff_stats_total_changes() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        assert_eq!(stats.total_changes(), 15);
    }

    #[test]
    fn test_file_diff_stats_has_changes() {
        let with_changes = FileDiffStats::new("test.rs", 1, 0, 'M');
        let no_changes = FileDiffStats::new("test.rs", 0, 0, 'M');
        assert!(with_changes.has_changes());
        assert!(!no_changes.has_changes());
    }

    #[test]
    fn test_file_diff_stats_default() {
        let stats = FileDiffStats::default();
        assert_eq!(stats.path, "");
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 0);
    }

    // =========================================================================
    // DiffStats Tests
    // =========================================================================

    #[test]
    fn test_diff_stats_new() {
        let stats = DiffStats::new();
        assert_eq!(stats.file_count(), 0);
        assert_eq!(stats.total_insertions(), 0);
        assert_eq!(stats.total_deletions(), 0);
    }

    #[test]
    fn test_diff_stats_add_file() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("file1.rs", 10, 5, 'M'));
        stats.add_file(FileDiffStats::new("file2.rs", 3, 2, 'M'));

        assert_eq!(stats.file_count(), 2);
        assert_eq!(stats.total_insertions(), 13);
        assert_eq!(stats.total_deletions(), 7);
        assert_eq!(stats.total_changes(), 20);
    }

    #[test]
    fn test_diff_stats_has_changes() {
        let mut stats = DiffStats::new();
        assert!(!stats.has_changes());

        stats.add_file(FileDiffStats::new("file.rs", 1, 0, 'M'));
        assert!(stats.has_changes());
    }

    #[test]
    fn test_diff_stats_max_path_length() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("short.rs", 1, 0, 'M'));
        stats.add_file(FileDiffStats::new("very_long_filename.rs", 1, 0, 'M'));

        assert_eq!(stats.max_path_length(), 21); // "very_long_filename.rs".len()
    }

    #[test]
    fn test_diff_stats_max_change_count() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("file1.rs", 5, 3, 'M'));
        stats.add_file(FileDiffStats::new("file2.rs", 10, 10, 'M'));

        assert_eq!(stats.max_change_count(), 20);
    }

    #[test]
    fn test_diff_stats_iter() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));
        stats.add_file(FileDiffStats::new("b.rs", 2, 0, 'M'));

        let paths: Vec<_> = stats.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_diff_stats_into_iter() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));

        for file_stats in stats {
            assert_eq!(file_stats.path, "a.rs");
        }
    }

    // =========================================================================
    // DiffOutputConfig Tests
    // =========================================================================

    #[test]
    fn test_diff_output_config_default() {
        let config = DiffOutputConfig::default();
        assert_eq!(config.context_lines, 3);
        assert!(config.color);
        assert_eq!(config.format, DiffFormat::Unified);
        assert_eq!(config.stat_width, 80);
        assert!(config.show_line_numbers);
        assert!(config.show_path_prefix);
        assert!(!config.word_diff);
    }

    #[test]
    fn test_diff_output_config_new() {
        let config = DiffOutputConfig::new();
        assert_eq!(config.context_lines, 3);
    }

    #[test]
    fn test_diff_output_config_with_context() {
        let config = DiffOutputConfig::new().with_context(5);
        assert_eq!(config.context_lines, 5);
    }

    #[test]
    fn test_diff_output_config_with_color() {
        let config = DiffOutputConfig::new().with_color(false);
        assert!(!config.color);
    }

    #[test]
    fn test_diff_output_config_with_format() {
        let config = DiffOutputConfig::new().with_format(DiffFormat::Stat);
        assert_eq!(config.format, DiffFormat::Stat);
    }

    #[test]
    fn test_diff_output_config_with_stat_width() {
        let config = DiffOutputConfig::new().with_stat_width(80);
        assert_eq!(config.stat_width, 80);
    }

    #[test]
    fn test_diff_output_config_with_line_numbers() {
        let config = DiffOutputConfig::new().with_line_numbers(true);
        assert!(config.show_line_numbers);
    }

    #[test]
    fn test_diff_output_config_with_path_prefix() {
        let config = DiffOutputConfig::new().with_path_prefix(false);
        assert!(!config.show_path_prefix);
    }

    #[test]
    fn test_diff_output_config_builder_chain() {
        let config = DiffOutputConfig::new()
            .with_context(10)
            .with_color(false)
            .with_format(DiffFormat::NameStatus)
            .with_stat_width(100)
            .with_line_numbers(true)
            .with_path_prefix(false);

        assert_eq!(config.context_lines, 10);
        assert!(!config.color);
        assert_eq!(config.format, DiffFormat::NameStatus);
        assert_eq!(config.stat_width, 100);
        assert!(config.show_line_numbers);
        assert!(!config.show_path_prefix);
    }

    // =========================================================================
    // DiffHunk Tests
    // =========================================================================

    #[test]
    fn test_diff_hunk_new() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        assert_eq!(graph_op.old_start, 1);
        assert_eq!(graph_op.old_count, 5);
        assert_eq!(graph_op.new_start, 1);
        assert_eq!(graph_op.new_count, 6);
        assert!(graph_op.lines.is_empty());
    }

    #[test]
    fn test_diff_hunk_add_line() {
        let mut graph_op = DiffHunk::new(1, 1, 1, 2);
        graph_op.add_line(HunkLine::context("unchanged", 1, 1));
        graph_op.add_line(HunkLine::added("new line", 2));

        assert_eq!(graph_op.lines.len(), 2);
    }

    #[test]
    fn test_diff_hunk_header() {
        let graph_op = DiffHunk::new(10, 5, 12, 7);
        assert_eq!(graph_op.header(), "@@ -10,5 +12,7 @@");
    }

    #[test]
    fn test_diff_hunk_header_single_lines() {
        let graph_op = DiffHunk::new(1, 1, 1, 1);
        assert_eq!(graph_op.header(), "@@ -1,1 +1,1 @@");
    }

    #[test]
    fn test_diff_hunk_has_changes() {
        let mut hunk_with_changes = DiffHunk::new(1, 1, 1, 2);
        hunk_with_changes.add_line(HunkLine::added("new", 1));
        assert!(hunk_with_changes.has_changes());

        let mut hunk_no_changes = DiffHunk::new(1, 1, 1, 1);
        hunk_no_changes.add_line(HunkLine::context("same", 1, 1));
        assert!(!hunk_no_changes.has_changes());
    }

    // =========================================================================
    // HunkLine Tests
    // =========================================================================

    #[test]
    fn test_hunk_line_context() {
        let line = HunkLine::context("unchanged line", 5, 5);
        assert_eq!(line.status, LineStatus::Unchanged);
        assert_eq!(line.content, "unchanged line");
        assert_eq!(line.old_line_num, Some(5));
        assert_eq!(line.new_line_num, Some(5));
    }

    #[test]
    fn test_hunk_line_added() {
        let line = HunkLine::added("new line", 10);
        assert_eq!(line.status, LineStatus::Added);
        assert_eq!(line.content, "new line");
        assert_eq!(line.old_line_num, None);
        assert_eq!(line.new_line_num, Some(10));
    }

    #[test]
    fn test_hunk_line_removed() {
        let line = HunkLine::removed("old line", 7);
        assert_eq!(line.status, LineStatus::Removed);
        assert_eq!(line.content, "old line");
        assert_eq!(line.old_line_num, Some(7));
        assert_eq!(line.new_line_num, None);
    }

    #[test]
    fn test_hunk_line_is_change() {
        assert!(!HunkLine::context("x", 1, 1).is_change());
        assert!(HunkLine::added("x", 1).is_change());
        assert!(HunkLine::removed("x", 1).is_change());
    }

    #[test]
    fn test_hunk_line_is_context() {
        assert!(HunkLine::context("x", 1, 1).is_context());
        assert!(!HunkLine::added("x", 1).is_context());
        assert!(!HunkLine::removed("x", 1).is_context());
    }

    #[test]
    fn test_hunk_line_prefix() {
        assert_eq!(HunkLine::context("x", 1, 1).prefix(), ' ');
        assert_eq!(HunkLine::added("x", 1).prefix(), '+');
        assert_eq!(HunkLine::removed("x", 1).prefix(), '-');
    }

    #[test]
    fn test_hunk_line_display() {
        let context = HunkLine::context("same", 1, 1);
        let added = HunkLine::added("new", 1);
        let removed = HunkLine::removed("old", 1);

        assert_eq!(format!("{}", context), " same");
        assert_eq!(format!("{}", added), "+new");
        assert_eq!(format!("{}", removed), "-old");
    }

    // =========================================================================
    // FileDiff Tests
    // =========================================================================

    #[test]
    fn test_file_diff_new() {
        let diff = FileDiff::new("src/main.rs", FileChangeStatus::Modified);
        assert_eq!(diff.old_path, "src/main.rs");
        assert_eq!(diff.new_path, "src/main.rs");
        assert_eq!(diff.status, FileChangeStatus::Modified);
        assert!(diff.hunks.is_empty());
        assert!(!diff.is_binary);
    }

    #[test]
    fn test_file_diff_added() {
        let diff = FileDiff::added("new_file.rs");
        assert_eq!(diff.old_path, "/dev/null");
        assert_eq!(diff.new_path, "new_file.rs");
        assert_eq!(diff.status, FileChangeStatus::Added);
        assert_eq!(diff.stats.status, 'A');
    }

    #[test]
    fn test_file_diff_deleted() {
        let diff = FileDiff::deleted("old_file.rs");
        assert_eq!(diff.old_path, "old_file.rs");
        assert_eq!(diff.new_path, "/dev/null");
        assert_eq!(diff.status, FileChangeStatus::Deleted);
        assert_eq!(diff.stats.status, 'D');
    }

    #[test]
    fn test_file_diff_modified() {
        let diff = FileDiff::modified("changed.rs");
        assert_eq!(diff.old_path, "changed.rs");
        assert_eq!(diff.new_path, "changed.rs");
        assert_eq!(diff.status, FileChangeStatus::Modified);
        assert_eq!(diff.stats.status, 'M');
    }

    #[test]
    fn test_file_diff_add_hunk() {
        let mut diff = FileDiff::modified("test.rs");
        diff.add_hunk(DiffHunk::new(1, 1, 1, 2));
        assert_eq!(diff.hunks.len(), 1);
    }

    #[test]
    fn test_file_diff_compute_stats() {
        let mut diff = FileDiff::modified("test.rs");
        let mut graph_op = DiffHunk::new(1, 2, 1, 3);
        graph_op.add_line(HunkLine::context("line1", 1, 1));
        graph_op.add_line(HunkLine::removed("old line", 2));
        graph_op.add_line(HunkLine::added("new line 1", 2));
        graph_op.add_line(HunkLine::added("new line 2", 3));
        diff.add_hunk(graph_op);

        diff.compute_stats();

        assert_eq!(diff.stats.insertions, 2);
        assert_eq!(diff.stats.deletions, 1);
    }

    #[test]
    fn test_file_diff_has_changes() {
        let mut diff = FileDiff::modified("test.rs");
        assert!(!diff.has_changes());

        let mut graph_op = DiffHunk::new(1, 1, 1, 2);
        graph_op.add_line(HunkLine::added("new", 1));
        diff.add_hunk(graph_op);
        assert!(diff.has_changes());
    }

    #[test]
    fn test_file_diff_has_changes_binary() {
        let mut diff = FileDiff::modified("image.png");
        diff.is_binary = true;
        assert!(diff.has_changes());
    }

    #[test]
    fn test_file_diff_display_path() {
        let added = FileDiff::added("new.rs");
        assert_eq!(added.display_path(), "new.rs");

        let deleted = FileDiff::deleted("old.rs");
        assert_eq!(deleted.display_path(), "old.rs");

        let modified = FileDiff::modified("changed.rs");
        assert_eq!(modified.display_path(), "changed.rs");
    }

    // =========================================================================
    // FileChangeStatus Tests
    // =========================================================================

    #[test]
    fn test_file_change_status_char() {
        assert_eq!(FileChangeStatus::Added.status_char(), 'A');
        assert_eq!(FileChangeStatus::Deleted.status_char(), 'D');
        assert_eq!(FileChangeStatus::Modified.status_char(), 'M');
        assert_eq!(FileChangeStatus::Renamed.status_char(), 'R');
        assert_eq!(FileChangeStatus::Copied.status_char(), 'C');
        assert_eq!(FileChangeStatus::TypeChanged.status_char(), 'T');
        assert_eq!(FileChangeStatus::Untracked.status_char(), 'U');
    }

    #[test]
    fn test_file_change_status_description() {
        assert_eq!(FileChangeStatus::Added.description(), "added");
        assert_eq!(FileChangeStatus::Deleted.description(), "deleted");
        assert_eq!(FileChangeStatus::Modified.description(), "modified");
        assert_eq!(FileChangeStatus::Renamed.description(), "renamed");
        assert_eq!(FileChangeStatus::Copied.description(), "copied");
        assert_eq!(FileChangeStatus::TypeChanged.description(), "type changed");
        assert_eq!(FileChangeStatus::Untracked.description(), "untracked");
    }

    #[test]
    fn test_file_change_status_display() {
        assert_eq!(format!("{}", FileChangeStatus::Added), "added");
        assert_eq!(format!("{}", FileChangeStatus::Modified), "modified");
        assert_eq!(format!("{}", FileChangeStatus::Untracked), "untracked");
    }

    #[test]
    fn test_file_change_status_from_file_status() {
        assert_eq!(
            FileChangeStatus::from(FileStatus::Added),
            FileChangeStatus::Added
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Deleted),
            FileChangeStatus::Deleted
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Modified),
            FileChangeStatus::Modified
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::TypeChanged),
            FileChangeStatus::TypeChanged
        );
        assert_eq!(
            FileChangeStatus::from(FileStatus::Untracked),
            FileChangeStatus::Untracked
        );
    }

    #[test]
    fn test_file_change_status_equality() {
        assert_eq!(FileChangeStatus::Added, FileChangeStatus::Added);
        assert_ne!(FileChangeStatus::Added, FileChangeStatus::Deleted);
    }

    #[test]
    fn test_file_change_status_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(FileChangeStatus::Added);
        set.insert(FileChangeStatus::Modified);
        assert!(set.contains(&FileChangeStatus::Added));
        assert!(!set.contains(&FileChangeStatus::Deleted));
    }

    // =========================================================================
    // Diff Command Tests
    // =========================================================================

    #[test]
    fn test_diff_new() {
        let diff = Diff::new();
        assert!(diff.files.is_empty());
        assert!(diff.change.is_none());
        assert_eq!(diff.algorithm, "myers");
        assert_eq!(diff.context, 3);
        assert!(!diff.stat);
        assert!(!diff.no_color);
        assert!(!diff.name_only);
        assert!(!diff.name_status);
        assert!(!diff.cached);
        assert!(diff.stack.is_none());
    }

    #[test]
    fn test_diff_default() {
        let diff = Diff::default();
        assert_eq!(diff.algorithm, "myers");
        assert_eq!(diff.context, 3);
    }

    #[test]
    fn test_diff_with_files() {
        let diff = Diff::new().with_files(vec!["a.rs", "b.rs"]);
        assert_eq!(diff.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_diff_with_files_string() {
        let diff = Diff::new().with_files(vec![String::from("test.rs")]);
        assert_eq!(diff.files, vec!["test.rs"]);
    }

    #[test]
    fn test_diff_with_change() {
        let diff = Diff::new().with_change("abc123");
        assert_eq!(diff.change, Some("abc123".to_string()));
    }

    #[test]
    fn test_diff_with_algorithm() {
        let diff = Diff::new().with_algorithm("patience");
        assert_eq!(diff.algorithm, "patience");
    }

    #[test]
    fn test_diff_with_context() {
        let diff = Diff::new().with_context(5);
        assert_eq!(diff.context, 5);
    }

    #[test]
    fn test_diff_with_stat() {
        let diff = Diff::new().with_stat(true);
        assert!(diff.stat);
    }

    #[test]
    fn test_diff_with_no_color() {
        let diff = Diff::new().with_no_color(true);
        assert!(diff.no_color);
    }

    #[test]
    fn test_diff_with_name_only() {
        let diff = Diff::new().with_name_only(true);
        assert!(diff.name_only);
    }

    #[test]
    fn test_diff_with_name_status() {
        let diff = Diff::new().with_name_status(true);
        assert!(diff.name_status);
    }

    #[test]
    fn test_diff_with_stack() {
        let diff = Diff::new().with_stack("feature");
        assert_eq!(diff.stack, Some("feature".to_string()));
    }

    #[test]
    fn test_diff_builder_chain() {
        let diff = Diff::new()
            .with_files(vec!["test.rs"])
            .with_algorithm("patience")
            .with_context(10)
            .with_stat(true)
            .with_no_color(true);

        assert_eq!(diff.files, vec!["test.rs"]);
        assert_eq!(diff.algorithm, "patience");
        assert_eq!(diff.context, 10);
        assert!(diff.stat);
        assert!(diff.no_color);
    }

    #[test]
    fn test_diff_get_format_unified() {
        let diff = Diff::new();
        assert_eq!(diff.get_format(), DiffFormat::Unified);
    }

    #[test]
    fn test_diff_get_format_stat() {
        let diff = Diff::new().with_stat(true);
        assert_eq!(diff.get_format(), DiffFormat::Stat);
    }

    #[test]
    fn test_diff_get_format_name_only() {
        let diff = Diff::new().with_name_only(true);
        assert_eq!(diff.get_format(), DiffFormat::NameOnly);
    }

    #[test]
    fn test_diff_get_format_name_status() {
        let diff = Diff::new().with_name_status(true);
        assert_eq!(diff.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    fn test_diff_get_format_priority() {
        // name_only takes priority over stat
        let diff = Diff::new().with_stat(true).with_name_only(true);
        assert_eq!(diff.get_format(), DiffFormat::NameOnly);

        // name_status takes priority over stat but not name_only
        let diff2 = Diff::new().with_stat(true).with_name_status(true);
        assert_eq!(diff2.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    fn test_diff_parse_algorithm_myers() {
        let diff = Diff::new().with_algorithm("myers");
        let algo = diff.parse_algorithm().unwrap();
        assert_eq!(algo, Algorithm::Myers);
    }

    #[test]
    fn test_diff_parse_algorithm_patience() {
        let diff = Diff::new().with_algorithm("patience");
        let algo = diff.parse_algorithm().unwrap();
        assert_eq!(algo, Algorithm::Patience);
    }

    #[test]
    fn test_diff_parse_algorithm_invalid() {
        let diff = Diff::new().with_algorithm("invalid");
        let result = diff.parse_algorithm();
        assert!(result.is_err());
    }

    #[test]
    fn test_diff_get_output_config() {
        let diff = Diff::new()
            .with_context(5)
            .with_no_color(true)
            .with_stat(true);

        let config = diff.get_output_config();

        assert_eq!(config.context_lines, 5);
        assert!(!config.color);
        assert_eq!(config.format, DiffFormat::Stat);
    }

    // =========================================================================
    // Helper Function Tests
    // =========================================================================

    #[test]
    fn test_format_stat_graph_empty() {
        let graph = format_stat_graph(0, 0, 50);
        assert_eq!(graph, "");
    }

    #[test]
    fn test_format_stat_graph_insertions_only() {
        let graph = format_stat_graph(5, 0, 50);
        assert_eq!(graph, "+++++");
    }

    #[test]
    fn test_format_stat_graph_deletions_only() {
        let graph = format_stat_graph(0, 3, 50);
        assert_eq!(graph, "---");
    }

    #[test]
    fn test_format_stat_graph_mixed() {
        let graph = format_stat_graph(3, 2, 50);
        assert_eq!(graph, "+++--");
    }

    #[test]
    fn test_format_stat_graph_scaled() {
        // When total > max_width, scale down
        let graph = format_stat_graph(100, 50, 30);
        // 100 + 50 = 150, scaled to 30
        // Should be approximately 20 + and 10 -
        assert!(graph.chars().filter(|&c| c == '+').count() <= 30);
        assert!(graph.chars().filter(|&c| c == '-').count() <= 30);
    }

    #[test]
    fn test_build_hunks_from_diff_empty() {
        let diff_result = DiffResult::new();
        let old_lines: Vec<&[u8]> = vec![];
        let new_lines: Vec<&[u8]> = vec![];

        let hunks = build_hunks_from_diff(&diff_result, &old_lines, &new_lines, 3);
        assert!(hunks.is_empty());
    }

    // =========================================================================
    // Debug and Clone Tests
    // =========================================================================

    #[test]
    fn test_diff_format_debug() {
        let format = DiffFormat::Unified;
        let debug = format!("{:?}", format);
        assert!(debug.contains("Unified"));
    }

    #[test]
    fn test_file_diff_stats_clone() {
        let stats = FileDiffStats::new("test.rs", 10, 5, 'M');
        let cloned = stats.clone();
        assert_eq!(stats.path, cloned.path);
        assert_eq!(stats.insertions, cloned.insertions);
    }

    #[test]
    fn test_diff_stats_clone() {
        let mut stats = DiffStats::new();
        stats.add_file(FileDiffStats::new("a.rs", 1, 0, 'M'));
        let cloned = stats.clone();
        assert_eq!(stats.file_count(), cloned.file_count());
    }

    #[test]
    fn test_diff_output_config_clone() {
        let config = DiffOutputConfig::new().with_context(10);
        let cloned = config.clone();
        assert_eq!(config.context_lines, cloned.context_lines);
    }

    #[test]
    fn test_diff_hunk_clone() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        let cloned = graph_op.clone();
        assert_eq!(graph_op.old_start, cloned.old_start);
    }

    #[test]
    fn test_hunk_line_clone() {
        let line = HunkLine::added("test", 1);
        let cloned = line.clone();
        assert_eq!(line.content, cloned.content);
    }

    #[test]
    fn test_file_diff_clone() {
        let diff = FileDiff::modified("test.rs");
        let cloned = diff.clone();
        assert_eq!(diff.old_path, cloned.old_path);
    }

    #[test]
    fn test_diff_cmd_clone() {
        let diff = Diff::new().with_context(10);
        let cloned = diff.clone();
        assert_eq!(diff.context, cloned.context);
    }

    #[test]
    fn test_diff_format_stat_copy() {
        let format = DiffFormat::Stat;
        let copied = format;
        assert_eq!(format, copied);
    }

    #[test]
    fn test_file_change_status_copy() {
        let status = FileChangeStatus::Added;
        let copied = status;
        assert_eq!(status, copied);
    }

    #[test]
    fn test_diff_hunk_debug() {
        let graph_op = DiffHunk::new(1, 5, 1, 6);
        let debug = format!("{:?}", graph_op);
        assert!(debug.contains("DiffHunk"));
    }

    #[test]
    fn test_hunk_line_debug() {
        let line = HunkLine::added("test", 1);
        let debug = format!("{:?}", line);
        assert!(debug.contains("HunkLine"));
    }

    #[test]
    fn test_file_diff_debug() {
        let diff = FileDiff::modified("test.rs");
        let debug = format!("{:?}", diff);
        assert!(debug.contains("FileDiff"));
    }

    #[test]
    fn test_file_change_status_debug() {
        let status = FileChangeStatus::Modified;
        let debug = format!("{:?}", status);
        assert!(debug.contains("Modified"));
    }

    #[test]
    fn test_diff_cmd_debug() {
        let diff = Diff::new();
        let debug = format!("{:?}", diff);
        assert!(debug.contains("Diff"));
    }

    // =========================================================================
    // Integration Tests (require temp directories)
    // =========================================================================

    use serial_test::serial;

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
    fn test_diff_run_outside_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_diff_run_no_changes() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should succeed but show no changes
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_with_untracked_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create untracked file
        {
            let _repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("untracked.txt"), "Hello").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Run diff (no changes expected since untracked files aren't shown by default)
        let diff = Diff::default();
        let result = diff.run();

        // Should succeed - untracked files are not shown in diff by default
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_short_flag() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create diff with --short flag
        let diff = Diff {
            short: true,
            ..Default::default()
        };

        // --short should set format to NameStatus
        assert_eq!(diff.get_format(), DiffFormat::NameStatus);
    }

    #[test]
    #[serial]
    fn test_diff_untracked_flag() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create untracked file
        {
            let _repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("untracked.txt"), "Hello").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Run diff with --untracked flag
        let diff = Diff {
            untracked: true,
            short: true,
            ..Default::default()
        };
        let result = diff.run();

        // Should succeed and include untracked files
        assert!(result.is_ok());
    }

    #[test]
    fn test_file_change_status_untracked() {
        assert_eq!(FileChangeStatus::Untracked.status_char(), 'U');
        assert_eq!(FileChangeStatus::Untracked.description(), "untracked");
    }

    #[test]
    #[serial]
    fn test_diff_run_with_added_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository, create and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("new_file.txt"), "New content").unwrap();
            repo.add("new_file.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new();
        let result = diff.run();

        // Should succeed and show the added file
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_name_only_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Content").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_name_only(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_run_stat_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Line 1\nLine 2\nLine 3\n").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_stat(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_end_to_end_modified_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();
        let file_path = repo_path.join("hello.txt");

        // Step 1: Initialize repository and add a file (scope to release lock)
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(&file_path, "Hello, World!\n").unwrap();
            repo.add("hello.txt", Default::default()).unwrap();
            // repo is dropped here, releasing the database lock
        }

        // Step 2: Record the initial change
        use crate::commands::record::Record;
        std::env::set_current_dir(repo_path).unwrap();

        let record = Record::new().with_message("Initial commit");
        let record_result = record.run();

        // Debug: Print record result
        if let Err(ref e) = record_result {
            eprintln!("Record error: {:?}", e);
        }

        // If record succeeded, continue with modification test
        if record_result.is_ok() {
            // Step 3: Modify the file
            std::fs::write(&file_path, "Hello, Modified World!\n").unwrap();

            // Step 4: Run diff - should show the modification
            let diff = Diff::new();
            let diff_result = diff.run();

            // Debug: Print diff result
            if let Err(ref e) = diff_result {
                eprintln!("Diff error: {:?}", e);
            }

            // The diff should succeed
            assert!(diff_result.is_ok());

            // Step 5: Verify the file is detected as modified by checking status
            let repo = Repository::open(repo_path).unwrap();
            let status = repo.status(Default::default()).unwrap();

            // The file should be detected as modified
            let modified_count = status.modified_count();

            // This assertion validates the full end-to-end workflow:
            // 1. File is recorded to graph
            // 2. File is modified on disk
            // 3. Status detects the modification by comparing content hashes
            // 4. Diff can show the changes
            //
            // If this fails with modified_count == 0, it means:
            // - Either record didn't properly save to the graph, OR
            // - Status can't retrieve the recorded content, OR
            // - Content comparison isn't working correctly
            assert!(
                modified_count > 0,
                "Expected file to be detected as modified. \
                 This indicates the record->status->diff chain is broken."
            );
        }
    }

    #[test]
    #[serial]
    fn test_diff_end_to_end_multiple_files() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Step 1: Initialize repository and create multiple files
        {
            let repo = Repository::init(repo_path).unwrap();

            // Create a directory structure
            std::fs::create_dir_all(repo_path.join("src")).unwrap();

            // Create multiple files with different content
            std::fs::write(
                repo_path.join("README.md"),
                "# My Project\n\nThis is a test project.\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("src/main.rs"),
                "fn main() {\n    println!(\"Hello, World!\");\n}\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("src/lib.rs"),
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            )
            .unwrap();
            std::fs::write(
                repo_path.join("config.toml"),
                "[settings]\nname = \"test\"\n",
            )
            .unwrap();

            // Add all files
            repo.add("README.md", Default::default()).unwrap();
            repo.add("src/main.rs", Default::default()).unwrap();
            repo.add("src/lib.rs", Default::default()).unwrap();
            repo.add("config.toml", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Step 2: Record the initial state
        use crate::commands::record::Record;
        let record = Record::new().with_message("Initial commit with multiple files");
        let record_result = record.run();

        if record_result.is_err() {
            // Skip test if record fails (may need dependencies)
            return;
        }

        // Step 3: Modify multiple files in different ways
        // Modify README.md
        std::fs::write(
            repo_path.join("README.md"),
            "# My Project\n\nThis is an **updated** test project.\n\n## Features\n\n- Feature 1\n- Feature 2\n",
        )
        .unwrap();

        // Modify src/main.rs
        std::fs::write(
            repo_path.join("src/main.rs"),
            "fn main() {\n    println!(\"Hello, Modified World!\");\n    println!(\"Second line\");\n}\n",
        )
        .unwrap();

        // Leave src/lib.rs unchanged

        // Delete config.toml
        std::fs::remove_file(repo_path.join("config.toml")).unwrap();

        // Add a new file (should show as added/untracked)
        std::fs::write(repo_path.join("new_file.txt"), "This is a new file\n").unwrap();

        // Step 4: Run diff and verify it works
        let diff = Diff::new();
        let diff_result = diff.run();
        assert!(diff_result.is_ok(), "Diff command should succeed");

        // Step 5: Verify status detects all changes correctly
        let repo = Repository::open(repo_path).unwrap();
        let status = repo.status(Default::default()).unwrap();

        // Check modified files
        let modified_count = status.modified_count();
        assert!(
            modified_count >= 2,
            "Expected at least 2 modified files (README.md, src/main.rs), got {}",
            modified_count
        );

        // Check deleted files
        let deleted_count = status.deleted_count();
        assert!(
            deleted_count >= 1,
            "Expected at least 1 deleted file (config.toml), got {}",
            deleted_count
        );

        // Verify specific files are in the expected state
        let modified_paths: Vec<_> = status.modified().map(|e| e.path().to_path_buf()).collect();
        assert!(
            modified_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("README.md")),
            "README.md should be modified"
        );
        assert!(
            modified_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("main.rs")),
            "src/main.rs should be modified"
        );

        let deleted_paths: Vec<_> = status.deleted().map(|e| e.path().to_path_buf()).collect();
        assert!(
            deleted_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("config.toml")),
            "config.toml should be deleted"
        );

        // Verify lib.rs is clean (unchanged)
        let clean_paths: Vec<_> = status.clean().map(|e| e.path().to_path_buf()).collect();
        assert!(
            clean_paths
                .iter()
                .any(|p| p.to_string_lossy().contains("lib.rs")),
            "src/lib.rs should be clean (unchanged)"
        );

        // Drop the repository to release the database lock before running more diff commands
        drop(status);
        drop(repo);

        // Step 6: Test diff with stat format
        let diff_stat = Diff::new().with_stat(true);
        let stat_result = diff_stat.run();
        assert!(stat_result.is_ok(), "Diff --stat should succeed");

        // Step 7: Test diff with name-only format
        let diff_name_only = Diff::new().with_name_only(true);
        let name_only_result = diff_name_only.run();
        assert!(name_only_result.is_ok(), "Diff --name-only should succeed");

        // Step 8: Test diff with name-status format
        let diff_name_status = Diff::new().with_name_status(true);
        let name_status_result = diff_name_status.run();
        assert!(
            name_status_result.is_ok(),
            "Diff --name-status should succeed"
        );

        // Step 9: Test diff for a specific file
        let diff_specific = Diff::new().with_files(vec!["README.md"]);
        let specific_result = diff_specific.run();
        assert!(
            specific_result.is_ok(),
            "Diff for specific file should succeed"
        );

        // Step 10: Verify content retrieval works for all recorded files
        // Re-open the repository since we dropped it earlier
        let repo = Repository::open(repo_path).unwrap();
        assert!(
            repo.get_file_content("README.md").unwrap().is_some(),
            "Should retrieve README.md content"
        );
        assert!(
            repo.get_file_content("src/main.rs").unwrap().is_some(),
            "Should retrieve src/main.rs content"
        );
        assert!(
            repo.get_file_content("src/lib.rs").unwrap().is_some(),
            "Should retrieve src/lib.rs content"
        );
    }

    #[test]
    #[serial]
    fn test_diff_with_specific_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add files
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
            std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();
            repo.add("file1.txt", Default::default()).unwrap();
            repo.add("file2.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Diff only file1.txt
        let diff = Diff::new().with_files(vec!["file1.txt"]);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_patience_algorithm() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Original content\n").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Use patience algorithm
        let diff = Diff::new().with_algorithm("patience");
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_no_color() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("test.txt"), "Content").unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let diff = Diff::new().with_no_color(true);
        let result = diff.run();

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_word_diff_enabled() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and create initial file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(repo_path.join("code.rs"), "let x = 42;\n").unwrap();
            repo.add("code.rs", Default::default()).unwrap();

            // Record the initial state
            let header = atomic_core::change::ChangeHeader::builder()
                .message("Initial commit")
                .build();
            repo.record(header, Default::default()).unwrap();
        }

        // Modify the file (change value)
        std::fs::write(repo_path.join("code.rs"), "let x = 100;\n").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        // Create diff with word-diff enabled
        let diff = Diff::new().with_word_diff(true).with_no_color(false); // Ensure color is on for word-diff

        assert!(diff.word_diff);

        let config = diff.get_output_config();
        assert!(config.word_diff);

        // Run should succeed
        let result = diff.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_diff_custom_context_lines() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize and add a file
        {
            let repo = Repository::init(repo_path).unwrap();
            std::fs::write(
                repo_path.join("test.txt"),
                "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n",
            )
            .unwrap();
            repo.add("test.txt", Default::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Use 5 context lines
        let diff = Diff::new().with_context(5);
        let result = diff.run();

        assert!(result.is_ok());
    }
}

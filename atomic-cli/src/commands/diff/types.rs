use super::*;

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

// Diff Statistics

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

    /// Status character (M/A/D/R/C).
    #[allow(dead_code)] // set in constructors, read via pattern match
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

    /// Check if this file has any changes.
    pub fn has_changes(&self) -> bool {
        self.insertions > 0 || self.deletions > 0
    }

    /// Check if this file was added.
    pub fn is_added(&self) -> bool {
        self.status == 'A'
    }

    /// Check if this file was deleted.
    pub fn is_deleted(&self) -> bool {
        self.status == 'D'
    }

    /// Check if this file was modified.
    pub fn is_modified(&self) -> bool {
        self.status == 'M'
    }
}

// Aggregate Diff Statistics

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

    /// Get total number of changed lines across all files.
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

// Diff Output Configuration

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

    /// Whether to enable word-level diff highlighting.
    #[allow(dead_code)] // set in Default impl
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
    /// Create a new config with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set the number of context lines.
    pub fn with_context(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Builder: set whether to use colored output.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Builder: set the output format.
    pub fn with_format(mut self, format: DiffFormat) -> Self {
        self.format = format;
        self
    }

    /// Builder: set the stat graph width.
    pub fn with_stat_width(mut self, width: usize) -> Self {
        self.stat_width = width;
        self
    }

    /// Builder: set whether to show line numbers.
    pub fn with_line_numbers(mut self, show: bool) -> Self {
        self.show_line_numbers = show;
        self
    }

    /// Builder: set whether to show path prefixes.
    pub fn with_path_prefix(mut self, show: bool) -> Self {
        self.show_path_prefix = show;
        self
    }

    /// Builder: set whether to enable word-level diff.
    pub fn with_word_diff(mut self, word_diff: bool) -> Self {
        self.word_diff = word_diff;
        self
    }
}

// Diff GraphOp

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

// GraphOp Line

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

    /// Check if this line was added.
    pub fn is_added(&self) -> bool {
        matches!(self.status, LineStatus::Added)
    }

    /// Check if this line was removed/deleted.
    pub fn is_removed(&self) -> bool {
        matches!(self.status, LineStatus::Removed)
    }

    /// Alias for `is_removed` — used in some test contexts.
    pub fn is_deleted(&self) -> bool {
        self.is_removed()
    }

    /// Check if this line is context (unchanged).
    pub fn is_context(&self) -> bool {
        matches!(self.status, LineStatus::Unchanged)
    }

    /// Check if this line is modified (alias for is_change).
    pub fn is_modified(&self) -> bool {
        self.is_change()
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

// File Diff

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
    /// Check if this file diff has any changes.
    pub fn has_changes(&self) -> bool {
        !self.hunks.is_empty()
            || self.stats.insertions > 0
            || self.stats.deletions > 0
            || self.is_binary
    }

    /// Get total number of changed lines (insertions + deletions).
    pub fn total_changes(&self) -> usize {
        self.stats.insertions + self.stats.deletions
    }

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

    /// Get the display path for the file.
    pub fn display_path(&self) -> &str {
        match self.status {
            FileChangeStatus::Added => &self.new_path,
            FileChangeStatus::Deleted => &self.old_path,
            _ => &self.new_path,
        }
    }
}

// File Change Status

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
    #[allow(dead_code)] // used in status_char/description match arms
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

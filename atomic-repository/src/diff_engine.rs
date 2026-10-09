//! The diff engine core: computing a change's file diffs — the shared
//! computation behind `atomic diff -c`, the triage report's embedded
//! hunks, and any other surface that needs real code hunks.
//!
//! Moved here (from the CLI's diff command) so the canonical triage report
//! builder could run in the repository layer: it is pure over the change
//! plus an optional before-content resolver (`change_file_diffs_with`) —
//! the CLI passes the graph's recorded before-content; a wire path may
//! pass `None` and get zero-context hunks at true offsets. The CLI's diff
//! command re-exports these and keeps only its presentation layers.

use std::str::FromStr;
use std::{cmp, fmt};

use atomic_core::change::Change;
use atomic_core::crdt::{BranchOp, LeafOp, TrunkOp};
use atomic_core::diff::display::LineStatus;
use atomic_core::diff::{diff_text, Algorithm, DiffOp, DiffResult};
use atomic_core::record::workflow::GitDiffLine;
use atomic_core::types::Hash;
use serde::Deserialize;

use crate::status::FileStatus;
use crate::Repository;

/// Output format for the diff command.
///
/// Controls how the diff output is presented to the user. Each format
/// serves different use cases from human review to scripting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
            show_line_numbers: false,
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
            crate::status::FileStatus::Added => FileChangeStatus::Added,
            crate::status::FileStatus::Deleted => FileChangeStatus::Deleted,
            crate::status::FileStatus::Modified => FileChangeStatus::Modified,
            crate::status::FileStatus::TypeChanged => FileChangeStatus::TypeChanged,
            crate::status::FileStatus::Clean => FileChangeStatus::Modified, // Shouldn't happen
            crate::status::FileStatus::Untracked => FileChangeStatus::Untracked,
            crate::status::FileStatus::Conflicted => FileChangeStatus::Modified,
            crate::status::FileStatus::PermissionsChanged => FileChangeStatus::Modified,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitMetadataDiffFile {
    path: String,
    lines: Vec<GitDiffLine>,
}

/// A changed line paired with the insert/delete offset preceding it.
///
/// `net_before` is (insertions - deletions) before this line, which is what
/// maps the line between old-file and new-file coordinates: an added line
/// `n` was inserted after old line `n - 1 - net_before`, and a removed line
/// `l` sat at new-file boundary `l - 1 + net_before`.
#[derive(Debug, Clone)]
pub struct NumberedLine {
    pub line: HunkLine,
    pub net_before: isize,
}

impl NumberedLine {
    pub fn added(content: impl Into<String>, new_num: usize, net_before: isize) -> Self {
        Self {
            line: HunkLine::added(content, new_num),
            net_before,
        }
    }

    pub fn removed(content: impl Into<String>, old_num: usize, net_before: isize) -> Self {
        Self {
            line: HunkLine::removed(content, old_num),
            net_before,
        }
    }

    /// Old-file position: the line number for removals, or the insertion
    /// boundary (the number of the preceding old line) for additions.
    fn old_pos(&self) -> usize {
        match self.line.status {
            LineStatus::Removed => self.line.old_line_num.unwrap_or(1),
            _ => {
                (self.line.new_line_num.unwrap_or(1) as isize - 1 - self.net_before).max(0) as usize
            }
        }
    }

    /// New-file position: the line number for additions, or the deletion
    /// boundary (the number of the preceding new line) for removals.
    fn new_pos(&self) -> usize {
        match self.line.status {
            LineStatus::Added => self.line.new_line_num.unwrap_or(1),
            _ => {
                (self.line.old_line_num.unwrap_or(1) as isize - 1 + self.net_before).max(0) as usize
            }
        }
    }
}

/// The engine's hunks-from-changed-lines core, as a free function (the
/// CLI's `impl Diff` carries a thin wrapper).
/// Group numbered changed lines into diff hunks with true file offsets.
///
/// `changed` must be in file order. Changes separated by more than
/// `2 * context` unchanged lines become separate hunks; closer changes
/// merge with the gap emitted as context. Context line content is taken
/// from `before_lines` (the file's recorded before-content); when it is
/// `None`, hunks are emitted with zero context but still carry true
/// offsets derived from the stored line numbers.
///
/// Hunk headers follow git's unified-diff conventions: a pure insertion
/// at boundary `p` is `@@ -p,0 +n,k @@`, a pure deletion is
/// `@@ -l,k +p,0 @@`, and context lines count toward both sides.
pub fn hunks_from_changed_lines(
    changed: &[NumberedLine],
    before_lines: Option<&[Vec<u8>]>,
    context: usize,
) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    if changed.is_empty() {
        return hunks;
    }

    let old_len = before_lines.map(|l| l.len()).unwrap_or(0);
    let ctx = if before_lines.is_some() { context } else { 0 };
    let merge_gap = 2 * ctx as isize;

    // 1. Group into runs of nearby changes, measured in old-file
    //    coordinates.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut run_start = 0usize;
    let mut run_hi = changed[0].old_pos();
    for (i, nl) in changed.iter().enumerate().skip(1) {
        let lo = nl.old_pos();
        // Unchanged lines between the run and this change. An addition
        // sits at a boundary, so the gap is measured from the boundary
        // itself; a removal occupies a line, hence the extra -1.
        let gap = match nl.line.status {
            LineStatus::Removed => lo as isize - run_hi as isize - 1,
            _ => lo as isize - run_hi as isize,
        };
        if gap > merge_gap {
            runs.push((run_start, i));
            run_start = i;
        }
        run_hi = run_hi.max(nl.old_pos());
    }
    runs.push((run_start, changed.len()));

    // 2. Emit one hunk per run.
    for (start, end) in runs {
        let lines = &changed[start..end];

        let removed_count = lines.iter().filter(|l| l.line.is_removed()).count();
        let added_count = lines.len() - removed_count;

        let old_lo = lines.iter().map(|l| l.old_pos()).min().unwrap_or(0);
        let old_hi = lines.iter().map(|l| l.old_pos()).max().unwrap_or(0);
        let new_lo = lines.iter().map(|l| l.new_pos()).min().unwrap_or(0);

        // Old-file range covered by the hunk, including context.
        let (old_start, old_end, old_count) = if removed_count > 0 {
            let s = old_lo.saturating_sub(ctx).max(1);
            let e = if ctx > 0 {
                old_hi.saturating_add(ctx).min(old_len)
            } else {
                old_hi
            };
            let e = e.max(s);
            (s, e, e - s + 1)
        } else {
            // Pure insertion at boundary old_lo. With context, the old
            // side covers the surviving lines around the boundary;
            // without, it is the boundary itself with a count of 0.
            let s = (old_lo + 1).saturating_sub(ctx).max(1);
            let e = if ctx > 0 {
                old_hi.saturating_add(ctx).min(old_len)
            } else {
                0
            };
            if ctx > 0 && s <= e {
                (s, e, e - s + 1)
            } else {
                (old_lo, old_lo, 0)
            }
        };

        // Emit context and changed lines in file order. `offset` is the
        // running old→new line shift used to number context lines.
        let mut hunk_lines: Vec<HunkLine> = Vec::new();
        let mut offset: isize = lines[0].net_before;
        let mut ctx_cursor = old_start;
        let mut pre_ctx = 0usize;

        for (idx, nl) in lines.iter().enumerate() {
            if ctx > 0 {
                let fill_end = match nl.line.status {
                    LineStatus::Removed => nl.old_pos().saturating_sub(1),
                    _ => nl.old_pos(),
                };
                while ctx_cursor <= fill_end && ctx_cursor <= old_end {
                    if let Some(content) = before_lines.and_then(|l| l.get(ctx_cursor - 1)) {
                        let new_num = (ctx_cursor as isize + offset).max(1) as usize;
                        hunk_lines.push(HunkLine::context(
                            String::from_utf8_lossy(content).into_owned(),
                            ctx_cursor,
                            new_num,
                        ));
                        if idx == 0 {
                            pre_ctx += 1;
                        }
                    }
                    ctx_cursor += 1;
                }
            }

            hunk_lines.push(nl.line.clone());
            match nl.line.status {
                LineStatus::Removed => {
                    offset -= 1;
                    ctx_cursor = ctx_cursor.max(nl.old_pos() + 1);
                }
                _ => {
                    offset += 1;
                }
            }
        }

        // Trailing context.
        if ctx > 0 {
            while ctx_cursor <= old_end {
                if let Some(content) = before_lines.and_then(|l| l.get(ctx_cursor - 1)) {
                    let new_num = (ctx_cursor as isize + offset).max(1) as usize;
                    hunk_lines.push(HunkLine::context(
                        String::from_utf8_lossy(content).into_owned(),
                        ctx_cursor,
                        new_num,
                    ));
                }
                ctx_cursor += 1;
            }
        }

        let context_count = hunk_lines.iter().filter(|l| l.is_context()).count();
        let new_count = context_count + added_count;
        let new_start = if new_count == 0 {
            // Pure deletion: anchor at the new-file boundary preceding
            // the deleted lines.
            new_lo
        } else {
            // New-file position of the first changed line, minus the
            // context lines emitted before it.
            let first = &lines[0];
            let anchor = match first.line.status {
                LineStatus::Removed => first.new_pos() + 1,
                _ => first.line.new_line_num.unwrap_or(1),
            };
            anchor.saturating_sub(pre_ctx).max(1)
        };

        let mut hunk = DiffHunk::new(old_start, old_count, new_start, new_count);
        hunk.lines = hunk_lines;
        hunks.push(hunk);
    }

    hunks
}

/// Reconstruct a line's text content from its leaf operations.
pub fn reconstruct_line_from_leaf_ops(leaf_ops: &[LeafOp]) -> String {
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

fn is_git_import_change(change: &Change) -> bool {
    change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .is_some()
}

pub fn build_git_import_file_diffs(change: &Change) -> Option<(Vec<FileDiff>, DiffStats)> {
    let diff_files_value = change
        .unhashed
        .as_ref()?
        .get("git")?
        .get("diff_lines")?
        .clone();

    let diff_files: Vec<GitMetadataDiffFile> = serde_json::from_value(diff_files_value).ok()?;
    let mut file_diffs = Vec::new();
    let mut stats = DiffStats::new();

    for entry in diff_files {
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        let mut hunk_lines = Vec::new();
        let mut old_start = None;
        let mut new_start = None;

        for line in entry.lines {
            let content = String::from_utf8_lossy(&line.content)
                .trim_end_matches('\n')
                .to_string();
            match line.origin {
                '+' => {
                    let new_num = line.new_lineno.unwrap_or((insertions + 1) as u32) as usize;
                    if new_start.is_none() {
                        new_start = Some(new_num);
                    }
                    hunk_lines.push(HunkLine::added(content, new_num));
                    insertions += 1;
                }
                '-' => {
                    let old_num = line.old_lineno.unwrap_or((deletions + 1) as u32) as usize;
                    if old_start.is_none() {
                        old_start = Some(old_num);
                    }
                    hunk_lines.push(HunkLine::removed(content, old_num));
                    deletions += 1;
                }
                _ => {}
            }
        }

        if hunk_lines.is_empty() {
            continue;
        }

        let status = match (insertions > 0, deletions > 0) {
            (true, false) => FileChangeStatus::Added,
            (false, true) => FileChangeStatus::Deleted,
            _ => FileChangeStatus::Modified,
        };

        let mut file_diff = match status {
            FileChangeStatus::Added => FileDiff::added(&entry.path),
            FileChangeStatus::Deleted => FileDiff::deleted(&entry.path),
            _ => FileDiff::modified(&entry.path),
        };

        let mut hunk = DiffHunk::new(
            old_start.unwrap_or(1),
            deletions,
            new_start.unwrap_or(1),
            insertions,
        );
        hunk.lines = hunk_lines;
        file_diff.add_hunk(hunk);
        file_diff.stats = match status {
            FileChangeStatus::Added => FileDiffStats::added(&entry.path, insertions),
            FileChangeStatus::Deleted => FileDiffStats::deleted(&entry.path, deletions),
            _ => FileDiffStats::modified(&entry.path, insertions, deletions),
        };

        stats.add_file(file_diff.stats.clone());
        file_diffs.push(file_diff);
    }

    Some((file_diffs, stats))
}

/// Re-pair Delete+Insert lines by content similarity for display.
///
/// The CRDT builder emits all Deletes before all Inserts within each
/// Replace block (to preserve BRANCH_AFTER chain ordering).  When the
/// Myers diff creates multiple Replace blocks for one file, a deleted
/// line and its matching insertion may land in different blocks.
///
/// This post-pass collects ALL Removed and Added lines across the
/// entire hunk, pairs them globally by bigram Jaccard similarity
/// (≥ 0.3 threshold), then re-emits lines in original order with
/// each paired Added line pulled forward to appear immediately after
/// its matching Removed line.
fn repair_diff_lines(lines: Vec<HunkLine>) -> Vec<HunkLine> {
    use std::collections::{HashMap, HashSet};

    // Helper: compute character bigrams for Jaccard similarity
    fn bigrams(s: &str) -> HashSet<(u8, u8)> {
        let bytes = s.trim().as_bytes();
        let mut set = HashSet::new();
        if bytes.len() >= 2 {
            for w in bytes.windows(2) {
                set.insert((w[0], w[1]));
            }
        }
        set
    }

    fn jaccard(a: &HashSet<(u8, u8)>, b: &HashSet<(u8, u8)>) -> f64 {
        if a.is_empty() && b.is_empty() {
            return 0.0;
        }
        let inter = a.intersection(b).count();
        let union = a.union(b).count();
        if union == 0 {
            0.0
        } else {
            inter as f64 / union as f64
        }
    }

    // 1. Collect all Removed and Added lines with their original indices
    let mut rm_entries: Vec<(usize, &HunkLine)> = Vec::new();
    let mut add_entries: Vec<(usize, &HunkLine)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if line.is_removed() {
            rm_entries.push((idx, line));
        } else if line.is_added() {
            add_entries.push((idx, line));
        }
    }

    // Short-circuit: nothing to pair
    if rm_entries.is_empty() || add_entries.is_empty() {
        return lines;
    }

    // 2. Compute bigrams
    let rm_bigrams: Vec<HashSet<(u8, u8)>> = rm_entries
        .iter()
        .map(|(_, l)| bigrams(&l.content))
        .collect();
    let add_bigrams: Vec<HashSet<(u8, u8)>> = add_entries
        .iter()
        .map(|(_, l)| bigrams(&l.content))
        .collect();

    // 3. Greedy best-match pairing across ALL removes and adds
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
    for (ri, rb) in rm_bigrams.iter().enumerate() {
        if rb.is_empty() {
            continue;
        }
        for (ai, ab) in add_bigrams.iter().enumerate() {
            if ab.is_empty() {
                continue;
            }
            let score = jaccard(rb, ab);
            if score >= 0.3 {
                candidates.push((ri, ai, score));
            }
        }
    }
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut matched_rm: HashSet<usize> = HashSet::new();
    let mut matched_add: HashSet<usize> = HashSet::new();
    // Map: original index of removed line → original index of added line
    let mut rm_to_add: HashMap<usize, usize> = HashMap::new();
    // Set of original indices of added lines that have been paired
    let mut paired_add_indices: HashSet<usize> = HashSet::new();

    for (ri, ai, _score) in &candidates {
        if matched_rm.contains(ri) || matched_add.contains(ai) {
            continue;
        }
        let rm_orig_idx = rm_entries[*ri].0;
        let add_orig_idx = add_entries[*ai].0;

        // Only pull an add forward, never backward — the add must come
        // after the remove in the original order, or "pulling it
        // forward" would instead duplicate a line already emitted at
        // its earlier original position.
        //
        // Intervening Added lines between the remove and its matched add
        // are allowed: pairing readability (showing what a buried
        // modification actually changed) matters more here than
        // preserving their exact relative display position, which is
        // the whole point of this heuristic — see the "buried
        // modification" and "pairing at scale" cases this exists for.
        if add_orig_idx <= rm_orig_idx {
            continue;
        }

        matched_rm.insert(*ri);
        matched_add.insert(*ai);
        rm_to_add.insert(rm_orig_idx, add_orig_idx);
        paired_add_indices.insert(add_orig_idx);
    }

    // Short-circuit: no pairs found
    if rm_to_add.is_empty() {
        return lines;
    }

    // 4. Re-emit lines in original order, pulling paired adds forward
    let mut result = Vec::with_capacity(lines.len());

    for (idx, line) in lines.iter().enumerate() {
        // Skip added lines that were already emitted after their pair
        if paired_add_indices.contains(&idx) {
            continue;
        }

        result.push(line.clone());

        // If this is a Removed line with a pair, emit the paired add
        if let Some(&add_idx) = rm_to_add.get(&idx) {
            result.push(lines[add_idx].clone());
        }
    }

    result
}

pub fn build_hunks_from_diff(
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

/// Build the real per-file unified diff for a recorded change WITHOUT printing.
///
/// This is the computation `atomic diff -c <hash>` runs, factored out so other
/// surfaces (e.g. the triage report) can embed actual code hunks instead of
/// re-implementing diffing. It returns every changed file's [`FileDiff`] plus
/// the aggregate [`DiffStats`], using the semantic layer (FileOps) with true
/// hunk offsets and before-content context, or Git's captured `+/-` lines for
/// git-imported changes.
///
/// It applies NO positional file filter (it has no `Diff` instance); the
/// `atomic diff` path applies `filter_file_diffs` afterward, which yields the
/// same result as the previous in-loop filtering.
pub fn change_file_diffs(
    repo: &Repository,
    change: &Change,
    change_hash: &Hash,
    config: &DiffOutputConfig,
) -> Result<(Vec<FileDiff>, DiffStats), crate::error::RepositoryError> {
    // The before-content fetch: the file's recorded state prior to the
    // change, read from the graph. The wire path has no such fetch — it
    // passes a `None` resolver and the hunks render zero-context, the
    // same degradation this path applies when the read fails.
    change_file_diffs_with(change, config, |path| {
        repo.get_file_content_before_change(path, change_hash)
            .ok()
            .flatten()
    })
}

/// Build the legacy (no-file_ops) change's file diffs from per-file
/// before/after content — the computation `atomic diff -c` runs for
/// legacy changes, factored out so the local body (graph-read content)
/// and the routed hook (wire-carried content) run the SAME code:
/// added (no before), deleted (no after), or modified (diffed with the
/// configured algorithm and context).
pub fn legacy_content_file_diffs(
    entries: &[(String, Vec<u8>, Vec<u8>)],
    algorithm: Algorithm,
    context_lines: usize,
) -> (Vec<FileDiff>, DiffStats) {
    let mut file_diffs = Vec::new();
    let mut stats = DiffStats::new();

    for (file_path, before_content, after_content) in entries {
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
                let diff_result = diff_text(before_content, after_content, algorithm);

                if !diff_result.is_unchanged() {
                    let old_lines: Vec<_> = before_content.split(|&b| b == b'\n').collect();
                    let new_lines: Vec<_> = after_content.split(|&b| b == b'\n').collect();

                    // Build hunks with context
                    let hunks =
                        build_hunks_from_diff(&diff_result, &old_lines, &new_lines, context_lines);
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

    (file_diffs, stats)
}

/// The resolver-parameterized core of [`change_file_diffs`]: `before`
/// supplies a file's pre-change content for context padding (a `None`
/// answer pads nothing — zero-context hunks, still at true offsets).
pub fn change_file_diffs_with<F>(
    change: &Change,
    config: &DiffOutputConfig,
    before: F,
) -> Result<(Vec<FileDiff>, DiffStats), crate::error::RepositoryError>
where
    F: Fn(&str) -> Option<Vec<u8>>,
{
    // Git-imported changes carry Git's captured +/- lines in unhashed metadata.
    if let Some((file_diffs, stats)) = build_git_import_file_diffs(change) {
        return Ok((file_diffs, stats));
    }

    let file_ops = change.file_ops();

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

        // Pass 1: flatten line operations into numbered changed lines.
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        let mut new_line_num = 1usize;
        let mut old_line_num = 1usize;
        let mut net: isize = 0;
        let mut changed: Vec<NumberedLine> = Vec::new();

        for line_op in line_ops {
            match line_op.operation() {
                BranchOp::Insert { content, .. } => {
                    let line_content = reconstruct_line_from_leaf_ops(content);
                    let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                    changed.push(NumberedLine::added(line_content, line_num, net));
                    new_line_num = line_num + 1;
                    insertions += 1;
                    net += 1;
                }
                BranchOp::Delete { content, .. } => {
                    let line_content = if content.is_empty() {
                        String::from("<deleted line>")
                    } else {
                        reconstruct_line_from_leaf_ops(content)
                    };
                    let line_num = line_op.old_line_num().unwrap_or(old_line_num);
                    changed.push(NumberedLine::removed(line_content, line_num, net));
                    old_line_num = line_num + 1;
                    deletions += 1;
                    net -= 1;
                }
                BranchOp::Modify {
                    old_content,
                    new_content,
                    ..
                } => {
                    let old_line_content = if old_content.is_empty() {
                        String::from("<modified line>")
                    } else {
                        reconstruct_line_from_leaf_ops(old_content)
                    };
                    let new_line_content = reconstruct_line_from_leaf_ops(new_content);

                    let old_ln = line_op.old_line_num().unwrap_or(old_line_num);
                    let new_ln = line_op.new_line_num().unwrap_or(new_line_num);

                    changed.push(NumberedLine::removed(old_line_content, old_ln, net));
                    net -= 1;
                    changed.push(NumberedLine::added(new_line_content, new_ln, net));
                    net += 1;

                    old_line_num = old_ln + 1;
                    new_line_num = new_ln + 1;
                    deletions += 1;
                    insertions += 1;
                }
                BranchOp::Restore { .. } => {
                    let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                    changed.push(NumberedLine::added(
                        String::from("<restored line>"),
                        line_num,
                        net,
                    ));
                    new_line_num = line_num + 1;
                    insertions += 1;
                    net += 1;
                }
                BranchOp::Reparent { .. } => {
                    // Position-only change — no visible delta in unified output.
                }
            }
        }

        // Fetch the file's before-content so hunks can be padded with
        // context lines. Only needed for unified output with a non-zero
        // --context; a `None` answer degrades to zero context.
        let before_lines: Option<Vec<Vec<u8>>> =
            if config.format == DiffFormat::Unified && config.context_lines > 0 {
                before(file_path)
                    .map(|content| content.split(|&b| b == b'\n').map(|l| l.to_vec()).collect())
            } else {
                None
            };

        // Pass 2: group changed lines into hunks at their true file offsets.
        let mut hunks =
            hunks_from_changed_lines(&changed, before_lines.as_deref(), config.context_lines);

        for hunk in &mut hunks {
            // Re-pair Delete+Insert lines by content similarity (skipped for
            // git-imports, which carry Git's authoritative line ordering).
            if !is_git_import_change(change) {
                hunk.lines = repair_diff_lines(std::mem::take(&mut hunk.lines));
            }
        }

        for hunk in hunks {
            file_diff.add_hunk(hunk);
        }

        file_diff.stats = match change_status {
            FileChangeStatus::Added => FileDiffStats::added(file_path, insertions),
            FileChangeStatus::Deleted => FileDiffStats::deleted(file_path, deletions),
            _ => FileDiffStats::modified(file_path, insertions, deletions),
        };

        stats.add_file(file_diff.stats.clone());
        file_diffs.push(file_diff);
    }

    Ok((file_diffs, stats))
}

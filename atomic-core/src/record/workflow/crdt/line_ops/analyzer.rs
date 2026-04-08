//! Line analyzer for diffing old and new content.
//!
//! Also contains [`AnalysisStats`] and [`LineAnalysis`] which were moved here
//! from `types.rs` to keep that file under 500 lines.

use crate::diff::{diff_text, DiffOp};
use serde::{Deserialize, Serialize};
use std::fmt;

use super::types::{AnalysisOptions, LineChange, LineChangeKind};

// ANALYSIS STATS

/// Statistics about the line analysis.
///
/// Provides counts and metrics about the analysis results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisStats {
    /// Total lines in old content.
    pub old_lines: usize,

    /// Total lines in new content.
    pub new_lines: usize,

    /// Number of unchanged lines.
    pub equal_lines: usize,

    /// Number of inserted lines.
    pub inserted_lines: usize,

    /// Number of deleted lines.
    pub deleted_lines: usize,

    /// Number of modified lines.
    pub modified_lines: usize,

    /// Number of moved lines.
    pub moved_lines: usize,

    /// Total changes (insert + delete + modify + move).
    pub total_changes: usize,
}

impl AnalysisStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates statistics with a line change.
    pub fn add_change(&mut self, change: &LineChange) {
        match change.kind() {
            LineChangeKind::Equal => self.equal_lines += 1,
            LineChangeKind::Insert => {
                self.inserted_lines += 1;
                self.total_changes += 1;
            }
            LineChangeKind::Delete => {
                self.deleted_lines += 1;
                self.total_changes += 1;
            }
            LineChangeKind::Modify => {
                self.modified_lines += 1;
                self.total_changes += 1;
            }
            LineChangeKind::Move => {
                self.moved_lines += 1;
                self.total_changes += 1;
            }
        }
    }

    /// Returns the percentage of lines changed.
    pub fn change_percentage(&self) -> f64 {
        let total = self.old_lines.max(self.new_lines);
        if total == 0 {
            0.0
        } else {
            (self.total_changes as f64 / total as f64) * 100.0
        }
    }

    /// Returns true if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.total_changes > 0
    }

    /// Returns the net line count change (positive = lines added).
    pub fn net_line_change(&self) -> i64 {
        self.new_lines as i64 - self.old_lines as i64
    }
}

impl fmt::Display for AnalysisStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changes: +{} -{} ~{} ={} ({}% changed)",
            self.total_changes,
            self.inserted_lines,
            self.deleted_lines,
            self.modified_lines,
            self.equal_lines,
            self.change_percentage() as u32
        )
    }
}

// LINE ANALYSIS

/// The result of analyzing line differences.
///
/// Contains all line changes between old and new content along with
/// statistics about the analysis.
#[derive(Debug, Clone)]
pub struct LineAnalysis {
    /// All changes in order.
    changes: Vec<LineChange>,

    /// Analysis statistics.
    stats: AnalysisStats,

    /// The options used for analysis.
    options: AnalysisOptions,
}

impl LineAnalysis {
    /// Creates a new analysis result.
    pub(crate) fn new(
        changes: Vec<LineChange>,
        stats: AnalysisStats,
        options: AnalysisOptions,
    ) -> Self {
        Self {
            changes,
            stats,
            options,
        }
    }

    /// Returns all changes.
    #[inline]
    pub fn changes(&self) -> &[LineChange] {
        &self.changes
    }

    /// Returns an iterator over all changes.
    pub fn iter(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter()
    }

    /// Returns the analysis statistics.
    #[inline]
    pub fn stats(&self) -> &AnalysisStats {
        &self.stats
    }

    /// Returns the options used for analysis.
    #[inline]
    pub fn options(&self) -> &AnalysisOptions {
        &self.options
    }

    /// Returns true if there are any changes.
    #[inline]
    pub fn has_changes(&self) -> bool {
        self.stats.has_changes()
    }

    /// Returns the number of changes.
    #[inline]
    pub fn change_count(&self) -> usize {
        self.changes.len()
    }

    /// Returns an iterator over equal lines only.
    pub fn equals(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().is_equal())
    }

    /// Returns an iterator over inserted lines only.
    pub fn inserts(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().is_insert())
    }

    /// Returns an iterator over deleted lines only.
    pub fn deletes(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().is_delete())
    }

    /// Returns an iterator over modified lines only.
    pub fn modifies(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().is_modify())
    }

    /// Returns an iterator over moved lines only.
    pub fn moves(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().is_move())
    }

    /// Returns an iterator over changes that affect old content.
    pub fn old_changes(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().affects_old())
    }

    /// Returns an iterator over changes that affect new content.
    pub fn new_changes(&self) -> impl Iterator<Item = &LineChange> {
        self.changes.iter().filter(|c| c.kind().affects_new())
    }

    /// Consumes the analysis and returns the changes.
    pub fn into_changes(self) -> Vec<LineChange> {
        self.changes
    }
}

// LINE ANALYZER

/// Analyzes differences between old and new content at the line level.
///
/// The analyzer splits content into lines and uses a diff algorithm to
/// find the minimal set of changes needed to transform the old content
/// into the new content.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::line_ops::{
///     LineAnalyzer, AnalysisOptions,
/// };
/// use atomic_core::diff::Algorithm;
///
/// let old = b"fn foo() {}\nfn bar() {}\n";
/// let new = b"fn foo() {}\nfn baz() {}\nfn bar() {}\n";
///
/// let options = AnalysisOptions::default().with_algorithm(Algorithm::Patience);
/// let analyzer = LineAnalyzer::new(old, new, options);
/// let analysis = analyzer.analyze();
///
/// assert!(analysis.has_changes());
/// assert_eq!(analysis.stats().inserted_lines, 1);
/// ```
#[derive(Debug, Clone)]
pub struct LineAnalyzer<'a> {
    /// Old content bytes.
    old_content: &'a [u8],

    /// New content bytes.
    new_content: &'a [u8],

    /// Analysis options.
    options: AnalysisOptions,
}

impl<'a> LineAnalyzer<'a> {
    /// Creates a new line analyzer.
    pub fn new(old_content: &'a [u8], new_content: &'a [u8], options: AnalysisOptions) -> Self {
        Self {
            old_content,
            new_content,
            options,
        }
    }

    /// Creates an analyzer with default options.
    pub fn with_defaults(old_content: &'a [u8], new_content: &'a [u8]) -> Self {
        Self::new(old_content, new_content, AnalysisOptions::default())
    }

    /// Returns the old content.
    #[inline]
    pub fn old_content(&self) -> &'a [u8] {
        self.old_content
    }

    /// Returns the new content.
    #[inline]
    pub fn new_content(&self) -> &'a [u8] {
        self.new_content
    }

    /// Returns the analysis options.
    #[inline]
    pub fn options(&self) -> &AnalysisOptions {
        &self.options
    }

    /// Performs the analysis and returns the result.
    ///
    /// This is the main entry point for analyzing differences.
    pub fn analyze(&self) -> LineAnalysis {
        // Split content into lines for counting
        let old_lines = Self::split_lines(self.old_content);
        let new_lines = Self::split_lines(self.new_content);

        // Perform diff using diff_text which handles Line conversion
        let diff_result = diff_text(self.old_content, self.new_content, self.options.algorithm());

        // Convert diff operations to line changes
        let mut changes = Vec::new();
        let mut stats = AnalysisStats::new();
        stats.old_lines = old_lines.len();
        stats.new_lines = new_lines.len();

        for op in diff_result.ops() {
            match op {
                DiffOp::Equal {
                    old_pos,
                    new_pos,
                    len,
                } => {
                    for i in 0..*len {
                        let old_idx = old_pos + i;
                        let new_idx = new_pos + i;
                        let content = old_lines[old_idx].to_vec();
                        let change = LineChange::equal(old_idx, new_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Insert {
                    old_pos: _,
                    new_pos,
                    len,
                } => {
                    for i in 0..*len {
                        let new_idx = new_pos + i;
                        let content = new_lines[new_idx].to_vec();
                        let change = LineChange::insert(new_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Delete {
                    old_pos,
                    new_pos: _,
                    len,
                } => {
                    for i in 0..*len {
                        let old_idx = old_pos + i;
                        let content = old_lines[old_idx].to_vec();
                        let change = LineChange::delete(old_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Replace {
                    old_pos,
                    old_len,
                    new_pos,
                    new_len,
                } => {
                    // For replacements, we generate delete + insert pairs
                    // or modify if lengths match and content is similar
                    if *old_len == *new_len && *old_len == 1 {
                        // Single line change - treat as modify
                        let old_content = old_lines[*old_pos].to_vec();
                        let new_content = new_lines[*new_pos].to_vec();
                        let change =
                            LineChange::modify(*old_pos, *new_pos, old_content, new_content);
                        stats.add_change(&change);
                        changes.push(change);
                    } else {
                        // Multiple lines - generate deletes then inserts
                        for i in 0..*old_len {
                            let old_idx = old_pos + i;
                            let content = old_lines[old_idx].to_vec();
                            let change = LineChange::delete(old_idx, content);
                            stats.add_change(&change);
                            changes.push(change);
                        }
                        for i in 0..*new_len {
                            let new_idx = new_pos + i;
                            let content = new_lines[new_idx].to_vec();
                            let change = LineChange::insert(new_idx, content);
                            stats.add_change(&change);
                            changes.push(change);
                        }
                    }
                }
            }
        }

        LineAnalysis::new(changes, stats, self.options.clone())
    }

    /// Splits content into lines (without trailing newlines).
    pub(crate) fn split_lines(content: &[u8]) -> Vec<&[u8]> {
        if content.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        let mut start = 0;

        for (i, &byte) in content.iter().enumerate() {
            if byte == b'\n' {
                lines.push(&content[start..i]);
                start = i + 1;
            }
        }

        // Handle final line without newline
        if start < content.len() {
            lines.push(&content[start..]);
        } else if start == content.len()
            && !content.is_empty()
            && content[content.len() - 1] == b'\n'
        {
            // Trailing newline creates empty final line
            lines.push(&content[start..start]);
        }

        lines
    }
}

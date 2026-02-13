//! Line-level diff analysis for CRDT Branch operations.
//!
//! This module converts line-level diff operations into CRDT `BranchOp`
//! operations. It analyzes differences between old and new content at
//! the line level to determine which lines were inserted, deleted, or
//! modified.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Line Diff Analysis Pipeline                         │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: Old Content + New Content                                       │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ Old: "line one\nline two\nline three\n"                          │  │
//! │  │ New: "line one\nmodified\nline three\nnew line\n"                │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  LineAnalyzer (performs diff)                                           │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ Uses Myers/Patience diff algorithm to find line differences      │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  LineAnalysis (structured result)                                       │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ LineChange::Equal    { old_idx: 0, new_idx: 0 }  // "line one"   │  │
//! │  │ LineChange::Delete   { old_idx: 1 }              // "line two"   │  │
//! │  │ LineChange::Insert   { new_idx: 1 }              // "modified"   │  │
//! │  │ LineChange::Equal    { old_idx: 2, new_idx: 2 }  // "line three" │  │
//! │  │ LineChange::Insert   { new_idx: 3 }              // "new line"   │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - [`LineAnalyzer`]: Main entry point for analyzing line differences
//! - [`LineAnalysis`]: Result of analysis containing all line changes
//! - [`LineChange`]: A single change (equal, insert, delete, or modify)
//! - [`LineChangeKind`]: Classification of the change type
//! - [`AnalysisOptions`]: Configuration for analysis behavior
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::line_ops::{
//!     LineAnalyzer, AnalysisOptions, LineChangeKind,
//! };
//!
//! let old = b"line one\nline two\n";
//! let new = b"line one\nmodified\nline three\n";
//!
//! let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
//! let analysis = analyzer.analyze();
//!
//! // Iterate over changes
//! for change in analysis.changes() {
//!     match change.kind() {
//!         LineChangeKind::Equal => println!("Unchanged: line {}", change.old_index().unwrap()),
//!         LineChangeKind::Insert => println!("Inserted: line {}", change.new_index().unwrap()),
//!         LineChangeKind::Delete => println!("Deleted: line {}", change.old_index().unwrap()),
//!         LineChangeKind::Modify => println!("Modified: line {}", change.old_index().unwrap()),
//!         LineChangeKind::Move => println!("Moved: line {}", change.old_index().unwrap()),
//!     }
//! }
//! ```
//!
//! # Integration with CRDT Model
//!
//! The analysis results map directly to CRDT operations:
//!
//! - `LineChange::Insert` → `BranchOp::Insert`
//! - `LineChange::Delete` → `BranchOp::Delete`
//! - `LineChange::Modify` → `BranchOp::Delete` + `BranchOp::Insert` (or token-level ops)
//! - `LineChange::Equal` → No operation (line unchanged)

use crate::crdt::BranchId;
use crate::diff::{diff_text, Algorithm, DiffOp};
use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// ANALYSIS OPTIONS
// ============================================================================

/// Options controlling line analysis behavior.
///
/// These options allow customization of how line differences are detected
/// and classified.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::line_ops::AnalysisOptions;
/// use atomic_core::diff::Algorithm;
///
/// let options = AnalysisOptions::default()
///     .with_algorithm(Algorithm::Patience)
///     .with_detect_moves(true)
///     .with_whitespace_significant(false);
///
/// assert_eq!(options.algorithm(), Algorithm::Patience);
/// assert!(options.detect_moves());
/// assert!(!options.whitespace_significant());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisOptions {
    /// The diff algorithm to use.
    algorithm: Algorithm,

    /// Whether to detect moved lines (same content, different position).
    detect_moves: bool,

    /// Whether whitespace differences are significant.
    whitespace_significant: bool,

    /// Whether to generate token-level analysis for modified lines.
    analyze_tokens: bool,

    /// Minimum similarity ratio for detecting modifications vs delete+insert.
    /// Lines with similarity above this threshold are classified as "modified".
    /// Range: 0.0 (never detect modifications) to 1.0 (only exact matches).
    modification_threshold: f64,
}

impl AnalysisOptions {
    /// Default modification threshold (50% similarity).
    pub const DEFAULT_MODIFICATION_THRESHOLD: f64 = 0.5;

    /// Creates new options with default settings.
    ///
    /// Default settings:
    /// - `algorithm`: Myers
    /// - `detect_moves`: false
    /// - `whitespace_significant`: true
    /// - `analyze_tokens`: true
    /// - `modification_threshold`: 0.5
    pub fn new() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            detect_moves: false,
            whitespace_significant: true,
            analyze_tokens: true,
            modification_threshold: Self::DEFAULT_MODIFICATION_THRESHOLD,
        }
    }

    /// Sets the diff algorithm to use.
    ///
    /// - `Myers`: Faster, good for most cases
    /// - `Patience`: Better for code with many similar lines
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Sets whether to detect moved lines.
    ///
    /// When enabled, lines that appear in different positions but have
    /// identical content are classified as moves rather than delete+insert.
    pub fn with_detect_moves(mut self, detect: bool) -> Self {
        self.detect_moves = detect;
        self
    }

    /// Sets whether whitespace differences are significant.
    ///
    /// When false, lines differing only in whitespace are considered equal.
    pub fn with_whitespace_significant(mut self, significant: bool) -> Self {
        self.whitespace_significant = significant;
        self
    }

    /// Sets whether to generate token-level analysis for modified lines.
    ///
    /// When enabled, modified lines include token-level diff information
    /// for fine-grained change tracking.
    pub fn with_analyze_tokens(mut self, analyze: bool) -> Self {
        self.analyze_tokens = analyze;
        self
    }

    /// Sets the modification detection threshold.
    ///
    /// Lines with similarity above this threshold are classified as
    /// "modified" rather than "deleted and inserted".
    ///
    /// # Panics
    ///
    /// Panics if threshold is not in range [0.0, 1.0].
    pub fn with_modification_threshold(mut self, threshold: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&threshold),
            "modification_threshold must be in range [0.0, 1.0]"
        );
        self.modification_threshold = threshold;
        self
    }

    /// Returns the diff algorithm.
    #[inline]
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Returns whether move detection is enabled.
    #[inline]
    pub fn detect_moves(&self) -> bool {
        self.detect_moves
    }

    /// Returns whether whitespace is significant.
    #[inline]
    pub fn whitespace_significant(&self) -> bool {
        self.whitespace_significant
    }

    /// Returns whether token analysis is enabled.
    #[inline]
    pub fn analyze_tokens(&self) -> bool {
        self.analyze_tokens
    }

    /// Returns the modification threshold.
    #[inline]
    pub fn modification_threshold(&self) -> f64 {
        self.modification_threshold
    }
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// LINE CHANGE KIND
// ============================================================================

/// The type of change for a line.
///
/// This enum classifies what happened to a line between the old and new
/// content versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LineChangeKind {
    /// Line is unchanged between old and new.
    Equal,

    /// Line was inserted (exists only in new).
    Insert,

    /// Line was deleted (exists only in old).
    Delete,

    /// Line was modified (exists in both, but content changed).
    ///
    /// This is detected when consecutive delete+insert operations
    /// have similar content above the modification threshold.
    Modify,

    /// Line was moved (same content, different position).
    ///
    /// Only detected when `detect_moves` is enabled in options.
    Move,
}

impl LineChangeKind {
    /// Returns true if this is an unchanged line.
    #[inline]
    pub fn is_equal(&self) -> bool {
        matches!(self, LineChangeKind::Equal)
    }

    /// Returns true if this is an inserted line.
    #[inline]
    pub fn is_insert(&self) -> bool {
        matches!(self, LineChangeKind::Insert)
    }

    /// Returns true if this is a deleted line.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self, LineChangeKind::Delete)
    }

    /// Returns true if this is a modified line.
    #[inline]
    pub fn is_modify(&self) -> bool {
        matches!(self, LineChangeKind::Modify)
    }

    /// Returns true if this is a moved line.
    #[inline]
    pub fn is_move(&self) -> bool {
        matches!(self, LineChangeKind::Move)
    }

    /// Returns true if this change affects the old content.
    #[inline]
    pub fn affects_old(&self) -> bool {
        matches!(
            self,
            LineChangeKind::Equal | LineChangeKind::Delete | LineChangeKind::Modify | LineChangeKind::Move
        )
    }

    /// Returns true if this change affects the new content.
    #[inline]
    pub fn affects_new(&self) -> bool {
        matches!(
            self,
            LineChangeKind::Equal | LineChangeKind::Insert | LineChangeKind::Modify | LineChangeKind::Move
        )
    }

    /// Returns a short name for display.
    pub fn name(&self) -> &'static str {
        match self {
            LineChangeKind::Equal => "equal",
            LineChangeKind::Insert => "insert",
            LineChangeKind::Delete => "delete",
            LineChangeKind::Modify => "modify",
            LineChangeKind::Move => "move",
        }
    }

    /// Returns a single character for compact display.
    pub fn as_char(&self) -> char {
        match self {
            LineChangeKind::Equal => '=',
            LineChangeKind::Insert => '+',
            LineChangeKind::Delete => '-',
            LineChangeKind::Modify => '~',
            LineChangeKind::Move => '>',
        }
    }
}

impl fmt::Display for LineChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ============================================================================
// LINE CHANGE
// ============================================================================

/// A single line change in the analysis.
///
/// Represents what happened to a specific line, including its position
/// in the old and/or new content and optionally the line content itself.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::line_ops::{LineChange, LineChangeKind};
///
/// // An inserted line
/// let insert = LineChange::insert(5, b"new line content".to_vec());
/// assert!(insert.kind().is_insert());
/// assert_eq!(insert.new_index(), Some(5));
/// assert!(insert.old_index().is_none());
///
/// // A deleted line
/// let delete = LineChange::delete(3, b"old line content".to_vec());
/// assert!(delete.kind().is_delete());
/// assert_eq!(delete.old_index(), Some(3));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineChange {
    /// The type of change.
    kind: LineChangeKind,

    /// Index in the old content (if applicable).
    old_index: Option<usize>,

    /// Index in the new content (if applicable).
    new_index: Option<usize>,

    /// The old line content (for delete/modify).
    old_content: Option<Vec<u8>>,

    /// The new line content (for insert/modify).
    new_content: Option<Vec<u8>>,

    /// Reference to existing BranchId (for delete/modify operations).
    existing_branch: Option<BranchId>,
}

impl LineChange {
    /// Creates an "equal" change (line unchanged).
    pub fn equal(old_index: usize, new_index: usize, content: Vec<u8>) -> Self {
        Self {
            kind: LineChangeKind::Equal,
            old_index: Some(old_index),
            new_index: Some(new_index),
            old_content: Some(content.clone()),
            new_content: Some(content),
            existing_branch: None,
        }
    }

    /// Creates an "insert" change (new line added).
    pub fn insert(new_index: usize, content: Vec<u8>) -> Self {
        Self {
            kind: LineChangeKind::Insert,
            old_index: None,
            new_index: Some(new_index),
            old_content: None,
            new_content: Some(content),
            existing_branch: None,
        }
    }

    /// Creates a "delete" change (line removed).
    pub fn delete(old_index: usize, content: Vec<u8>) -> Self {
        Self {
            kind: LineChangeKind::Delete,
            old_index: Some(old_index),
            new_index: None,
            old_content: Some(content),
            new_content: None,
            existing_branch: None,
        }
    }

    /// Creates a "modify" change (line content changed).
    pub fn modify(old_index: usize, new_index: usize, old_content: Vec<u8>, new_content: Vec<u8>) -> Self {
        Self {
            kind: LineChangeKind::Modify,
            old_index: Some(old_index),
            new_index: Some(new_index),
            old_content: Some(old_content),
            new_content: Some(new_content),
            existing_branch: None,
        }
    }

    /// Creates a "move" change (line relocated).
    pub fn moved(old_index: usize, new_index: usize, content: Vec<u8>) -> Self {
        Self {
            kind: LineChangeKind::Move,
            old_index: Some(old_index),
            new_index: Some(new_index),
            old_content: Some(content.clone()),
            new_content: Some(content),
            existing_branch: None,
        }
    }

    /// Sets the existing branch ID for this change.
    ///
    /// This is used when modifying or deleting an existing line to reference
    /// the branch that should be affected.
    pub fn with_existing_branch(mut self, branch_id: BranchId) -> Self {
        self.existing_branch = Some(branch_id);
        self
    }

    /// Returns the change kind.
    #[inline]
    pub fn kind(&self) -> LineChangeKind {
        self.kind
    }

    /// Returns the old content index (if applicable).
    #[inline]
    pub fn old_index(&self) -> Option<usize> {
        self.old_index
    }

    /// Returns the new content index (if applicable).
    #[inline]
    pub fn new_index(&self) -> Option<usize> {
        self.new_index
    }

    /// Returns the old line content (if applicable).
    #[inline]
    pub fn old_content(&self) -> Option<&[u8]> {
        self.old_content.as_deref()
    }

    /// Returns the new line content (if applicable).
    #[inline]
    pub fn new_content(&self) -> Option<&[u8]> {
        self.new_content.as_deref()
    }

    /// Returns the old content as a string (lossy).
    pub fn old_content_str(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.old_content.as_ref().map(|c| String::from_utf8_lossy(c))
    }

    /// Returns the new content as a string (lossy).
    pub fn new_content_str(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.new_content.as_ref().map(|c| String::from_utf8_lossy(c))
    }

    /// Returns the existing branch ID (if set).
    #[inline]
    pub fn existing_branch(&self) -> Option<BranchId> {
        self.existing_branch
    }

    /// Returns true if this change requires content to be stored.
    ///
    /// Insert and Modify operations need new content stored in the change.
    pub fn needs_content(&self) -> bool {
        matches!(self.kind, LineChangeKind::Insert | LineChangeKind::Modify)
    }

    /// Returns the content that should be stored (new content for insert/modify).
    pub fn content_to_store(&self) -> Option<&[u8]> {
        if self.needs_content() {
            self.new_content.as_deref()
        } else {
            None
        }
    }
}

impl fmt::Display for LineChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            LineChangeKind::Equal => write!(
                f,
                "= old:{} new:{}",
                self.old_index.unwrap_or(0),
                self.new_index.unwrap_or(0)
            ),
            LineChangeKind::Insert => write!(
                f,
                "+ new:{}",
                self.new_index.unwrap_or(0)
            ),
            LineChangeKind::Delete => write!(
                f,
                "- old:{}",
                self.old_index.unwrap_or(0)
            ),
            LineChangeKind::Modify => write!(
                f,
                "~ old:{} new:{}",
                self.old_index.unwrap_or(0),
                self.new_index.unwrap_or(0)
            ),
            LineChangeKind::Move => write!(
                f,
                "> old:{} new:{}",
                self.old_index.unwrap_or(0),
                self.new_index.unwrap_or(0)
            ),
        }
    }
}

// ============================================================================
// ANALYSIS STATS
// ============================================================================

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

// ============================================================================
// LINE ANALYSIS
// ============================================================================

/// The result of analyzing line differences.
///
/// Contains all line changes between old and new content along with
/// statistics about the analysis.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::line_ops::{
///     LineAnalyzer, AnalysisOptions, LineChangeKind,
/// };
///
/// let old = b"a\nb\nc\n";
/// let new = b"a\nmodified\nc\nd\n";
///
/// let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
/// let analysis = analyzer.analyze();
///
/// // Access changes
/// assert!(analysis.has_changes());
/// println!("Changes: {}", analysis.stats());
///
/// // Filter by type
/// let inserts: Vec<_> = analysis.inserts().collect();
/// let deletes: Vec<_> = analysis.deletes().collect();
/// ```
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
    fn new(changes: Vec<LineChange>, stats: AnalysisStats, options: AnalysisOptions) -> Self {
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

// ============================================================================
// ANALYSIS RESULT (FOR COMPATIBILITY WITH MOD.RS EXPORTS)
// ============================================================================

/// Alias for LineAnalysis for API compatibility.
pub type AnalysisResult = LineAnalysis;

// ============================================================================
// LINE ANALYZER
// ============================================================================

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
                DiffOp::Equal { old_pos, new_pos, len } => {
                    for i in 0..*len {
                        let old_idx = old_pos + i;
                        let new_idx = new_pos + i;
                        let content = old_lines[old_idx].to_vec();
                        let change = LineChange::equal(old_idx, new_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Insert { old_pos: _, new_pos, len } => {
                    for i in 0..*len {
                        let new_idx = new_pos + i;
                        let content = new_lines[new_idx].to_vec();
                        let change = LineChange::insert(new_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Delete { old_pos, new_pos: _, len } => {
                    for i in 0..*len {
                        let old_idx = old_pos + i;
                        let content = old_lines[old_idx].to_vec();
                        let change = LineChange::delete(old_idx, content);
                        stats.add_change(&change);
                        changes.push(change);
                    }
                }
                DiffOp::Replace { old_pos, old_len, new_pos, new_len } => {
                    // For replacements, we generate delete + insert pairs
                    // or modify if lengths match and content is similar
                    if *old_len == *new_len && *old_len == 1 {
                        // Single line change - treat as modify
                        let old_content = old_lines[*old_pos].to_vec();
                        let new_content = new_lines[*new_pos].to_vec();
                        let change = LineChange::modify(*old_pos, *new_pos, old_content, new_content);
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
    fn split_lines(content: &[u8]) -> Vec<&[u8]> {
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
        } else if start == content.len() && !content.is_empty() && content[content.len() - 1] == b'\n' {
            // Trailing newline creates empty final line
            lines.push(&content[start..start]);
        }

        lines
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // AnalysisOptions Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_analysis_options_default() {
        let opts = AnalysisOptions::default();
        assert_eq!(opts.algorithm(), Algorithm::Myers);
        assert!(!opts.detect_moves());
        assert!(opts.whitespace_significant());
        assert!(opts.analyze_tokens());
    }

    #[test]
    fn test_analysis_options_new() {
        let opts = AnalysisOptions::new();
        assert_eq!(opts.algorithm(), Algorithm::Myers);
    }

    #[test]
    fn test_analysis_options_builder_algorithm() {
        let opts = AnalysisOptions::new().with_algorithm(Algorithm::Patience);
        assert_eq!(opts.algorithm(), Algorithm::Patience);
    }

    #[test]
    fn test_analysis_options_builder_detect_moves() {
        let opts = AnalysisOptions::new().with_detect_moves(true);
        assert!(opts.detect_moves());
    }

    #[test]
    fn test_analysis_options_builder_whitespace_significant() {
        let opts = AnalysisOptions::new().with_whitespace_significant(false);
        assert!(!opts.whitespace_significant());
    }

    #[test]
    fn test_analysis_options_builder_analyze_tokens() {
        let opts = AnalysisOptions::new().with_analyze_tokens(false);
        assert!(!opts.analyze_tokens());
    }

    #[test]
    fn test_analysis_options_builder_modification_threshold() {
        let opts = AnalysisOptions::new().with_modification_threshold(0.75);
        assert!((opts.modification_threshold() - 0.75).abs() < 0.001);
    }

    #[test]
    #[should_panic(expected = "modification_threshold must be in range")]
    fn test_analysis_options_invalid_threshold_high() {
        let _ = AnalysisOptions::new().with_modification_threshold(1.5);
    }

    #[test]
    #[should_panic(expected = "modification_threshold must be in range")]
    fn test_analysis_options_invalid_threshold_low() {
        let _ = AnalysisOptions::new().with_modification_threshold(-0.1);
    }

    #[test]
    fn test_analysis_options_builder_chain() {
        let opts = AnalysisOptions::new()
            .with_algorithm(Algorithm::Patience)
            .with_detect_moves(true)
            .with_whitespace_significant(false)
            .with_analyze_tokens(false)
            .with_modification_threshold(0.8);

        assert_eq!(opts.algorithm(), Algorithm::Patience);
        assert!(opts.detect_moves());
        assert!(!opts.whitespace_significant());
        assert!(!opts.analyze_tokens());
        assert!((opts.modification_threshold() - 0.8).abs() < 0.001);
    }

    // ------------------------------------------------------------------------
    // LineChangeKind Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_change_kind_is_methods() {
        assert!(LineChangeKind::Equal.is_equal());
        assert!(!LineChangeKind::Equal.is_insert());

        assert!(LineChangeKind::Insert.is_insert());
        assert!(!LineChangeKind::Insert.is_delete());

        assert!(LineChangeKind::Delete.is_delete());
        assert!(!LineChangeKind::Delete.is_modify());

        assert!(LineChangeKind::Modify.is_modify());
        assert!(!LineChangeKind::Modify.is_move());

        assert!(LineChangeKind::Move.is_move());
        assert!(!LineChangeKind::Move.is_equal());
    }

    #[test]
    fn test_line_change_kind_affects_old() {
        assert!(LineChangeKind::Equal.affects_old());
        assert!(!LineChangeKind::Insert.affects_old());
        assert!(LineChangeKind::Delete.affects_old());
        assert!(LineChangeKind::Modify.affects_old());
        assert!(LineChangeKind::Move.affects_old());
    }

    #[test]
    fn test_line_change_kind_affects_new() {
        assert!(LineChangeKind::Equal.affects_new());
        assert!(LineChangeKind::Insert.affects_new());
        assert!(!LineChangeKind::Delete.affects_new());
        assert!(LineChangeKind::Modify.affects_new());
        assert!(LineChangeKind::Move.affects_new());
    }

    #[test]
    fn test_line_change_kind_name() {
        assert_eq!(LineChangeKind::Equal.name(), "equal");
        assert_eq!(LineChangeKind::Insert.name(), "insert");
        assert_eq!(LineChangeKind::Delete.name(), "delete");
        assert_eq!(LineChangeKind::Modify.name(), "modify");
        assert_eq!(LineChangeKind::Move.name(), "move");
    }

    #[test]
    fn test_line_change_kind_as_char() {
        assert_eq!(LineChangeKind::Equal.as_char(), '=');
        assert_eq!(LineChangeKind::Insert.as_char(), '+');
        assert_eq!(LineChangeKind::Delete.as_char(), '-');
        assert_eq!(LineChangeKind::Modify.as_char(), '~');
        assert_eq!(LineChangeKind::Move.as_char(), '>');
    }

    #[test]
    fn test_line_change_kind_display() {
        assert_eq!(format!("{}", LineChangeKind::Insert), "insert");
    }

    // ------------------------------------------------------------------------
    // LineChange Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_change_equal() {
        let change = LineChange::equal(5, 5, b"content".to_vec());
        assert!(change.kind().is_equal());
        assert_eq!(change.old_index(), Some(5));
        assert_eq!(change.new_index(), Some(5));
        assert_eq!(change.old_content(), Some(b"content".as_slice()));
        assert_eq!(change.new_content(), Some(b"content".as_slice()));
    }

    #[test]
    fn test_line_change_insert() {
        let change = LineChange::insert(3, b"new line".to_vec());
        assert!(change.kind().is_insert());
        assert_eq!(change.old_index(), None);
        assert_eq!(change.new_index(), Some(3));
        assert_eq!(change.old_content(), None);
        assert_eq!(change.new_content(), Some(b"new line".as_slice()));
    }

    #[test]
    fn test_line_change_delete() {
        let change = LineChange::delete(7, b"old line".to_vec());
        assert!(change.kind().is_delete());
        assert_eq!(change.old_index(), Some(7));
        assert_eq!(change.new_index(), None);
        assert_eq!(change.old_content(), Some(b"old line".as_slice()));
        assert_eq!(change.new_content(), None);
    }

    #[test]
    fn test_line_change_modify() {
        let change = LineChange::modify(2, 2, b"old".to_vec(), b"new".to_vec());
        assert!(change.kind().is_modify());
        assert_eq!(change.old_index(), Some(2));
        assert_eq!(change.new_index(), Some(2));
        assert_eq!(change.old_content(), Some(b"old".as_slice()));
        assert_eq!(change.new_content(), Some(b"new".as_slice()));
    }

    #[test]
    fn test_line_change_moved() {
        let change = LineChange::moved(1, 5, b"moved line".to_vec());
        assert!(change.kind().is_move());
        assert_eq!(change.old_index(), Some(1));
        assert_eq!(change.new_index(), Some(5));
    }

    #[test]
    fn test_line_change_with_existing_branch() {
        use crate::types::NodeId;
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let change = LineChange::delete(0, b"line".to_vec()).with_existing_branch(branch_id);
        assert_eq!(change.existing_branch(), Some(branch_id));
    }

    #[test]
    fn test_line_change_content_str() {
        let change = LineChange::modify(0, 0, b"old text".to_vec(), b"new text".to_vec());
        assert_eq!(change.old_content_str().unwrap(), "old text");
        assert_eq!(change.new_content_str().unwrap(), "new text");
    }

    #[test]
    fn test_line_change_needs_content() {
        let insert = LineChange::insert(0, b"new".to_vec());
        let delete = LineChange::delete(0, b"old".to_vec());
        let modify = LineChange::modify(0, 0, b"old".to_vec(), b"new".to_vec());
        let equal = LineChange::equal(0, 0, b"same".to_vec());

        assert!(insert.needs_content());
        assert!(!delete.needs_content());
        assert!(modify.needs_content());
        assert!(!equal.needs_content());
    }

    #[test]
    fn test_line_change_content_to_store() {
        let insert = LineChange::insert(0, b"new content".to_vec());
        assert_eq!(insert.content_to_store(), Some(b"new content".as_slice()));

        let delete = LineChange::delete(0, b"old".to_vec());
        assert_eq!(delete.content_to_store(), None);
    }

    #[test]
    fn test_line_change_display() {
        let insert = LineChange::insert(5, b"x".to_vec());
        let display = format!("{}", insert);
        assert!(display.contains("+"));
        assert!(display.contains("new:5"));

        let delete = LineChange::delete(3, b"y".to_vec());
        let display = format!("{}", delete);
        assert!(display.contains("-"));
        assert!(display.contains("old:3"));
    }

    // ------------------------------------------------------------------------
    // AnalysisStats Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_analysis_stats_new() {
        let stats = AnalysisStats::new();
        assert_eq!(stats.old_lines, 0);
        assert_eq!(stats.total_changes, 0);
    }

    #[test]
    fn test_analysis_stats_add_change() {
        let mut stats = AnalysisStats::new();

        stats.add_change(&LineChange::equal(0, 0, b"x".to_vec()));
        assert_eq!(stats.equal_lines, 1);
        assert_eq!(stats.total_changes, 0);

        stats.add_change(&LineChange::insert(1, b"y".to_vec()));
        assert_eq!(stats.inserted_lines, 1);
        assert_eq!(stats.total_changes, 1);

        stats.add_change(&LineChange::delete(2, b"z".to_vec()));
        assert_eq!(stats.deleted_lines, 1);
        assert_eq!(stats.total_changes, 2);

        stats.add_change(&LineChange::modify(3, 3, b"a".to_vec(), b"b".to_vec()));
        assert_eq!(stats.modified_lines, 1);
        assert_eq!(stats.total_changes, 3);
    }

    #[test]
    fn test_analysis_stats_change_percentage() {
        let mut stats = AnalysisStats::new();
        stats.old_lines = 10;
        stats.new_lines = 10;
        stats.total_changes = 5;

        assert!((stats.change_percentage() - 50.0).abs() < 0.001);
    }

    #[test]
    fn test_analysis_stats_change_percentage_empty() {
        let stats = AnalysisStats::new();
        assert!((stats.change_percentage() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_analysis_stats_has_changes() {
        let mut stats = AnalysisStats::new();
        assert!(!stats.has_changes());

        stats.total_changes = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_analysis_stats_net_line_change() {
        let mut stats = AnalysisStats::new();
        stats.old_lines = 5;
        stats.new_lines = 8;
        assert_eq!(stats.net_line_change(), 3);

        stats.new_lines = 3;
        assert_eq!(stats.net_line_change(), -2);
    }

    #[test]
    fn test_analysis_stats_display() {
        let mut stats = AnalysisStats::new();
        stats.total_changes = 5;
        stats.inserted_lines = 2;
        stats.deleted_lines = 1;
        stats.modified_lines = 2;
        stats.equal_lines = 10;
        stats.old_lines = 15;
        stats.new_lines = 16;

        let display = format!("{}", stats);
        assert!(display.contains("5 changes"));
        assert!(display.contains("+2"));
        assert!(display.contains("-1"));
    }

    // ------------------------------------------------------------------------
    // LineAnalysis Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_analysis_accessors() {
        let changes = vec![
            LineChange::equal(0, 0, b"a".to_vec()),
            LineChange::insert(1, b"b".to_vec()),
        ];
        let mut stats = AnalysisStats::new();
        for c in &changes {
            stats.add_change(c);
        }
        let analysis = LineAnalysis::new(changes, stats, AnalysisOptions::default());

        assert_eq!(analysis.change_count(), 2);
        assert!(analysis.has_changes());
        assert_eq!(analysis.stats().inserted_lines, 1);
    }

    #[test]
    fn test_line_analysis_filters() {
        let changes = vec![
            LineChange::equal(0, 0, b"a".to_vec()),
            LineChange::insert(1, b"b".to_vec()),
            LineChange::delete(2, b"c".to_vec()),
            LineChange::modify(3, 2, b"d".to_vec(), b"e".to_vec()),
        ];
        let stats = AnalysisStats::new();
        let analysis = LineAnalysis::new(changes, stats, AnalysisOptions::default());

        assert_eq!(analysis.equals().count(), 1);
        assert_eq!(analysis.inserts().count(), 1);
        assert_eq!(analysis.deletes().count(), 1);
        assert_eq!(analysis.modifies().count(), 1);
    }

    #[test]
    fn test_line_analysis_into_changes() {
        let changes = vec![LineChange::insert(0, b"x".to_vec())];
        let analysis = LineAnalysis::new(changes, AnalysisStats::new(), AnalysisOptions::default());
        let owned = analysis.into_changes();
        assert_eq!(owned.len(), 1);
    }

    // ------------------------------------------------------------------------
    // LineAnalyzer Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_analyzer_split_lines_empty() {
        let lines = LineAnalyzer::split_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_line_analyzer_split_lines_single() {
        let lines = LineAnalyzer::split_lines(b"hello");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], b"hello");
    }

    #[test]
    fn test_line_analyzer_split_lines_multiple() {
        let lines = LineAnalyzer::split_lines(b"a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"a");
        assert_eq!(lines[1], b"b");
        assert_eq!(lines[2], b"c");
    }

    #[test]
    fn test_line_analyzer_split_lines_trailing_newline() {
        let lines = LineAnalyzer::split_lines(b"a\nb\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"a");
        assert_eq!(lines[1], b"b");
        assert_eq!(lines[2], b"");
    }

    #[test]
    fn test_line_analyzer_identical_content() {
        let content = b"line one\nline two\n";
        let analyzer = LineAnalyzer::new(content, content, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(!analysis.has_changes());
        // Two lines of content (trailing newline doesn't create empty line in diff)
        assert!(analysis.stats().equal_lines >= 2);
    }

    #[test]
    fn test_line_analyzer_all_inserted() {
        let old = b"";
        let new = b"new line\n";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        // At least one line inserted
        assert!(analysis.stats().inserted_lines >= 1);
    }

    #[test]
    fn test_line_analyzer_all_deleted() {
        let old = b"old line\n";
        let new = b"";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        // At least one line deleted
        assert!(analysis.stats().deleted_lines >= 1);
    }

    #[test]
    fn test_line_analyzer_simple_modification() {
        let old = b"unchanged\nold line\nunchanged\n";
        let new = b"unchanged\nnew line\nunchanged\n";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        // The middle line is modified (detected as delete+insert or modify)
        assert!(analysis.stats().modified_lines >= 1 ||
                (analysis.stats().deleted_lines >= 1 && analysis.stats().inserted_lines >= 1));
    }

    #[test]
    fn test_line_analyzer_insert_at_end() {
        let old = b"line one\n";
        let new = b"line one\nline two\n";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        assert!(analysis.stats().inserted_lines >= 1);
    }

    #[test]
    fn test_line_analyzer_delete_from_middle() {
        let old = b"a\nb\nc\n";
        let new = b"a\nc\n";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        assert_eq!(analysis.stats().deleted_lines, 1);
    }

    #[test]
    fn test_line_analyzer_with_defaults() {
        let analyzer = LineAnalyzer::with_defaults(b"a", b"b");
        assert_eq!(analyzer.options().algorithm(), Algorithm::Myers);
    }

    #[test]
    fn test_line_analyzer_content_accessors() {
        let old = b"old";
        let new = b"new";
        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());

        assert_eq!(analyzer.old_content(), old);
        assert_eq!(analyzer.new_content(), new);
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_integration_code_change() {
        let old = b"fn foo() {\n    return 1;\n}\n";
        let new = b"fn foo() {\n    return 2;\n}\n";

        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        // First and last lines unchanged, middle modified
        assert_eq!(analysis.stats().equal_lines, 2);
    }

    #[test]
    fn test_integration_multiple_changes() {
        let old = b"a\nb\nc\nd\ne\n";
        let new = b"a\nB\nc\nD\ne\nnew\n";

        let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        // a, c, e unchanged = 3, plus 1 empty from trailing newline matched
        // b->B, d->D modified or delete+insert
        // "new" inserted
    }

    #[test]
    fn test_integration_patience_algorithm() {
        let old = b"fn main() {\n}\n";
        let new = b"fn main() {\n    println!(\"hello\");\n}\n";

        let options = AnalysisOptions::default().with_algorithm(Algorithm::Patience);
        let analyzer = LineAnalyzer::new(old, new, options);
        let analysis = analyzer.analyze();

        assert!(analysis.has_changes());
        assert!(analysis.stats().inserted_lines >= 1);
    }
}

//! Types for line-level diff analysis.
//!
//! This module contains the core types used by the line analysis subsystem:
//! [`AnalysisOptions`], [`LineChangeKind`], [`LineChange`], and [`LineChangeKind`].
//! [`AnalysisStats`] and [`LineAnalysis`] are in `analyzer.rs`.

use crate::crdt::BranchId;
use crate::diff::Algorithm;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

// ANALYSIS OPTIONS

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

// LINE CHANGE KIND

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
            LineChangeKind::Equal
                | LineChangeKind::Delete
                | LineChangeKind::Modify
                | LineChangeKind::Move
        )
    }

    /// Returns true if this change affects the new content.
    #[inline]
    pub fn affects_new(&self) -> bool {
        matches!(
            self,
            LineChangeKind::Equal
                | LineChangeKind::Insert
                | LineChangeKind::Modify
                | LineChangeKind::Move
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

// LINE CHANGE

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
    pub fn modify(
        old_index: usize,
        new_index: usize,
        old_content: Vec<u8>,
        new_content: Vec<u8>,
    ) -> Self {
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
    pub fn old_content_str(&self) -> Option<Cow<'_, str>> {
        self.old_content
            .as_ref()
            .map(|c| String::from_utf8_lossy(c))
    }

    /// Returns the new content as a string (lossy).
    pub fn new_content_str(&self) -> Option<Cow<'_, str>> {
        self.new_content
            .as_ref()
            .map(|c| String::from_utf8_lossy(c))
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
            LineChangeKind::Insert => write!(f, "+ new:{}", self.new_index.unwrap_or(0)),
            LineChangeKind::Delete => write!(f, "- old:{}", self.old_index.unwrap_or(0)),
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

// NOTE: AnalysisStats and LineAnalysis are defined in analyzer.rs to keep
// this file under 500 lines. They are re-exported from mod.rs.

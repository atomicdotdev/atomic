use super::*;

// SemanticDiff - The complete diff result

/// Configuration for semantic diff operations.
#[derive(Debug, Clone)]
pub struct SemanticDiffConfig {
    /// The line-level diff algorithm to use.
    pub algorithm: Algorithm,

    /// Configuration for token-level diffing.
    pub token_config: TokenizerConfig,

    /// Configuration for word-level diffing within lines.
    pub word_config: WordDiffConfig,

    /// Whether to include context lines (unchanged lines) in the output.
    pub include_context: bool,

    /// Number of context lines to include around changes.
    pub context_lines: usize,

    /// Whether to ignore whitespace-only changes.
    pub ignore_whitespace: bool,

    /// Whether to treat blank line changes as significant.
    pub ignore_blank_lines: bool,
}

impl Default for SemanticDiffConfig {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            token_config: TokenizerConfig::default(),
            word_config: WordDiffConfig::default(),
            include_context: false,
            context_lines: 3,
            ignore_whitespace: false,
            ignore_blank_lines: false,
        }
    }
}

impl SemanticDiffConfig {
    /// Create a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Include context lines in output.
    pub fn with_context(mut self, lines: usize) -> Self {
        self.include_context = true;
        self.context_lines = lines;
        self
    }

    /// Ignore whitespace-only changes.
    pub fn ignore_whitespace(mut self) -> Self {
        self.ignore_whitespace = true;
        self
    }

    /// Ignore blank line changes.
    pub fn ignore_blank_lines(mut self) -> Self {
        self.ignore_blank_lines = true;
        self
    }
}

/// Statistics about a semantic diff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticDiffStats {
    /// Number of lines added.
    pub lines_added: usize,

    /// Number of lines deleted.
    pub lines_deleted: usize,

    /// Number of lines modified.
    pub lines_modified: usize,

    /// Total number of tokens inserted.
    pub tokens_inserted: usize,

    /// Total number of tokens deleted.
    pub tokens_deleted: usize,

    /// Total number of tokens replaced.
    pub tokens_replaced: usize,
}

impl SemanticDiffStats {
    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.lines_added > 0
            || self.lines_deleted > 0
            || self.lines_modified > 0
            || self.tokens_inserted > 0
            || self.tokens_deleted > 0
            || self.tokens_replaced > 0
    }

    /// Get the total number of line changes.
    pub fn total_line_changes(&self) -> usize {
        self.lines_added + self.lines_deleted + self.lines_modified
    }

    /// Get the total number of token changes.
    pub fn total_token_changes(&self) -> usize {
        self.tokens_inserted + self.tokens_deleted + self.tokens_replaced
    }
}

impl fmt::Display for SemanticDiffStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} insertions(+), {} deletions(-), {} modifications(~), {} token changes",
            self.lines_added,
            self.lines_deleted,
            self.lines_modified,
            self.total_token_changes()
        )
    }
}

/// The result of a semantic diff operation.
///
/// Contains line-level changes with token-level detail for modified lines.
#[derive(Debug, Clone)]
pub struct SemanticDiff<'a> {
    /// The line changes in order.
    pub(super) changes: Vec<LineChange<'a>>,

    /// Statistics about the diff.
    pub(super) stats: SemanticDiffStats,

    /// The old lines (for reference).
    pub(super) old_lines: Vec<SemanticLine<'a>>,

    /// The new lines (for reference).
    pub(super) new_lines: Vec<SemanticLine<'a>>,
}

impl<'a> SemanticDiff<'a> {
    /// Create an empty semantic diff (no changes).
    pub fn empty() -> Self {
        Self {
            changes: Vec::new(),
            stats: SemanticDiffStats::default(),
            old_lines: Vec::new(),
            new_lines: Vec::new(),
        }
    }

    /// Get the line changes.
    #[inline]
    pub fn changes(&self) -> &[LineChange<'a>] {
        &self.changes
    }

    /// Get the diff statistics.
    #[inline]
    pub fn stats(&self) -> &SemanticDiffStats {
        &self.stats
    }

    /// Get the old lines.
    #[inline]
    pub fn old_lines(&self) -> &[SemanticLine<'a>] {
        &self.old_lines
    }

    /// Get the new lines.
    #[inline]
    pub fn new_lines(&self) -> &[SemanticLine<'a>] {
        &self.new_lines
    }

    /// Check if there are any changes.
    #[inline]
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Check if the content is unchanged.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        self.changes.is_empty()
    }

    /// Get the number of line changes.
    #[inline]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Check if there are no changes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Iterate over added lines.
    pub fn added_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_added())
    }

    /// Iterate over deleted lines.
    pub fn deleted_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_deleted())
    }

    /// Iterate over modified lines.
    pub fn modified_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_modified())
    }

    /// Get all token changes across all line changes.
    pub fn all_token_changes(&self) -> impl Iterator<Item = &TokenChange<'a>> {
        self.changes.iter().flat_map(|c| c.token_changes())
    }
}

impl<'a> fmt::Display for SemanticDiff<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unchanged() {
            return write!(f, "No changes");
        }

        writeln!(f, "{}", self.stats)?;
        for change in &self.changes {
            writeln!(f, "  {}", change)?;
        }
        Ok(())
    }
}

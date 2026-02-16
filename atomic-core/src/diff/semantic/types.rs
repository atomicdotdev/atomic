use super::*;

/// A line with pre-computed tokens for efficient diffing.
///
/// This struct holds both the raw line content and its tokenized form,
/// avoiding repeated tokenization during diff operations.
#[derive(Clone)]
pub struct SemanticLine<'a> {
    /// The raw line content.
    pub(super) line: Line<'a>,

    /// Pre-computed tokens for this line.
    pub(super) tokens: Vec<Token<'a>>,

    /// The original line number (1-indexed, for display).
    pub(super) line_num: usize,
}

impl<'a> SemanticLine<'a> {
    /// Create a new semantic line from raw content.
    ///
    /// Tokenizes the content immediately for later use.
    pub fn new(content: &'a [u8], line_num: usize) -> Self {
        let line = Line::new(content);
        let tokens = Tokenizer::new(content).collect();
        Self {
            line,
            tokens,
            line_num,
        }
    }

    /// Create a semantic line from an existing Line.
    pub fn from_line(line: Line<'a>, line_num: usize) -> Self {
        let tokens = Tokenizer::new(line.content()).collect();
        Self {
            line,
            tokens,
            line_num,
        }
    }

    /// Create semantic lines from byte content.
    pub fn from_bytes(content: &'a [u8]) -> Vec<Self> {
        Line::from_bytes(content)
            .into_iter()
            .enumerate()
            .map(|(i, line)| Self::from_line(line, i + 1))
            .collect()
    }

    /// Get the underlying Line.
    #[inline]
    pub fn line(&self) -> &Line<'a> {
        &self.line
    }

    /// Get the raw content of this line.
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.line.content()
    }

    /// Get the content as a string (lossy conversion for non-UTF8).
    pub fn content_str(&self) -> std::borrow::Cow<'a, str> {
        String::from_utf8_lossy(self.content())
    }

    /// Get the content without trailing newline.
    #[inline]
    pub fn content_without_newline(&self) -> &'a [u8] {
        self.line.content_without_newline()
    }

    /// Get the pre-computed tokens.
    #[inline]
    pub fn tokens(&self) -> &[Token<'a>] {
        &self.tokens
    }

    /// Get the line number (1-indexed).
    #[inline]
    pub fn line_num(&self) -> usize {
        self.line_num
    }

    /// Check if the line is empty (or only whitespace).
    pub fn is_blank(&self) -> bool {
        self.tokens
            .iter()
            .all(|t| t.kind() == TokenKind::Whitespace || t.kind() == TokenKind::Newline)
    }

    /// Get the number of significant (non-whitespace) tokens.
    pub fn significant_token_count(&self) -> usize {
        self.tokens
            .iter()
            .filter(|t| t.kind().is_significant())
            .count()
    }
}

impl<'a> fmt::Debug for SemanticLine<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticLine")
            .field("line_num", &self.line_num)
            .field("content", &self.content_str())
            .field("token_count", &self.tokens.len())
            .finish()
    }
}

impl<'a> PartialEq for SemanticLine<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line
    }
}

impl<'a> Eq for SemanticLine<'a> {}

// TokenChange - Changes within a single token

/// A change to a single token within a line.
///
/// This represents the finest granularity of change tracking - what
/// happened to a specific token (word, operator, etc.) within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenChange<'a> {
    /// Token is unchanged (context).
    Unchanged {
        /// The token content.
        token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },

    /// Token was inserted (only in new line).
    Inserted {
        /// The inserted token.
        token: Token<'a>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },

    /// Token was deleted (only in old line).
    Deleted {
        /// The deleted token.
        token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
    },

    /// Token was replaced (different in old and new).
    Replaced {
        /// The old token that was replaced.
        old_token: Token<'a>,
        /// The new token that replaced it.
        new_token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },
}

impl<'a> TokenChange<'a> {
    /// Check if this is an unchanged token.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, TokenChange::Unchanged { .. })
    }

    /// Check if this represents a change (insert, delete, or replace).
    #[inline]
    pub fn is_change(&self) -> bool {
        !self.is_unchanged()
    }

    /// Check if this is an insertion.
    #[inline]
    pub fn is_inserted(&self) -> bool {
        matches!(self, TokenChange::Inserted { .. })
    }

    /// Check if this is a deletion.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, TokenChange::Deleted { .. })
    }

    /// Check if this is a replacement.
    #[inline]
    pub fn is_replaced(&self) -> bool {
        matches!(self, TokenChange::Replaced { .. })
    }

    /// Get the old token if present.
    pub fn old_token(&self) -> Option<&Token<'a>> {
        match self {
            TokenChange::Unchanged { token, .. } => Some(token),
            TokenChange::Deleted { token, .. } => Some(token),
            TokenChange::Replaced { old_token, .. } => Some(old_token),
            TokenChange::Inserted { .. } => None,
        }
    }

    /// Get the new token if present.
    pub fn new_token(&self) -> Option<&Token<'a>> {
        match self {
            TokenChange::Unchanged { token, .. } => Some(token),
            TokenChange::Inserted { token, .. } => Some(token),
            TokenChange::Replaced { new_token, .. } => Some(new_token),
            TokenChange::Deleted { .. } => None,
        }
    }

    /// Get the byte range in the old line (if applicable).
    pub fn old_range(&self) -> Option<Range<usize>> {
        match self {
            TokenChange::Unchanged { old_range, .. } => Some(old_range.clone()),
            TokenChange::Deleted { old_range, .. } => Some(old_range.clone()),
            TokenChange::Replaced { old_range, .. } => Some(old_range.clone()),
            TokenChange::Inserted { .. } => None,
        }
    }

    /// Get the byte range in the new line (if applicable).
    pub fn new_range(&self) -> Option<Range<usize>> {
        match self {
            TokenChange::Unchanged { new_range, .. } => Some(new_range.clone()),
            TokenChange::Inserted { new_range, .. } => Some(new_range.clone()),
            TokenChange::Replaced { new_range, .. } => Some(new_range.clone()),
            TokenChange::Deleted { .. } => None,
        }
    }

    /// Get a display-friendly description of this change.
    pub fn description(&self) -> String {
        match self {
            TokenChange::Unchanged { token, .. } => {
                format!("unchanged: {:?}", token.as_str())
            }
            TokenChange::Inserted { token, .. } => {
                format!("inserted: {:?}", token.as_str())
            }
            TokenChange::Deleted { token, .. } => {
                format!("deleted: {:?}", token.as_str())
            }
            TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } => {
                format!(
                    "replaced: {:?} → {:?}",
                    old_token.as_str(),
                    new_token.as_str()
                )
            }
        }
    }
}

impl<'a> fmt::Display for TokenChange<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

// LineChange - Changes to a line

/// A change to a line in the diff.
///
/// This is the primary output type for semantic diffs. Each variant
/// describes what happened to a line and, for modified lines, includes
/// token-level detail about what changed within the line.
#[derive(Debug, Clone)]
pub enum LineChange<'a> {
    /// A line was added (only in new version).
    Added {
        /// Line number in the new version (1-indexed).
        line_num: usize,
        /// The added line with its tokens.
        line: SemanticLine<'a>,
        /// Token operations for this line (all insertions).
        tokens: Vec<TokenChange<'a>>,
    },

    /// A line was deleted (only in old version).
    Deleted {
        /// Line number in the old version (1-indexed).
        line_num: usize,
        /// The deleted line with its tokens.
        line: SemanticLine<'a>,
        /// Token operations for this line (all deletions).
        tokens: Vec<TokenChange<'a>>,
    },

    /// A line was modified (exists in both but different).
    Modified {
        /// Line number in the old version (1-indexed).
        old_line_num: usize,
        /// Line number in the new version (1-indexed).
        new_line_num: usize,
        /// The old version of the line.
        before: SemanticLine<'a>,
        /// The new version of the line.
        after: SemanticLine<'a>,
        /// Token-level changes within the line.
        token_changes: Vec<TokenChange<'a>>,
    },
}

impl<'a> LineChange<'a> {
    /// Check if this is an added line.
    #[inline]
    pub fn is_added(&self) -> bool {
        matches!(self, LineChange::Added { .. })
    }

    /// Check if this is a deleted line.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, LineChange::Deleted { .. })
    }

    /// Check if this is a modified line.
    #[inline]
    pub fn is_modified(&self) -> bool {
        matches!(self, LineChange::Modified { .. })
    }

    /// Get the old line number (if applicable).
    pub fn old_line_num(&self) -> Option<usize> {
        match self {
            LineChange::Deleted { line_num, .. } => Some(*line_num),
            LineChange::Modified { old_line_num, .. } => Some(*old_line_num),
            LineChange::Added { .. } => None,
        }
    }

    /// Get the new line number (if applicable).
    pub fn new_line_num(&self) -> Option<usize> {
        match self {
            LineChange::Added { line_num, .. } => Some(*line_num),
            LineChange::Modified { new_line_num, .. } => Some(*new_line_num),
            LineChange::Deleted { .. } => None,
        }
    }

    /// Get the token changes for this line change.
    pub fn token_changes(&self) -> &[TokenChange<'a>] {
        match self {
            LineChange::Added { tokens, .. } => tokens,
            LineChange::Deleted { tokens, .. } => tokens,
            LineChange::Modified { token_changes, .. } => token_changes,
        }
    }

    /// Count the number of token insertions.
    pub fn token_insertions(&self) -> usize {
        self.token_changes()
            .iter()
            .filter(|tc| tc.is_inserted() || tc.is_replaced())
            .count()
    }

    /// Count the number of token deletions.
    pub fn token_deletions(&self) -> usize {
        self.token_changes()
            .iter()
            .filter(|tc| tc.is_deleted() || tc.is_replaced())
            .count()
    }

    /// Get a summary of what changed.
    pub fn summary(&self) -> String {
        match self {
            LineChange::Added { line_num, line, .. } => {
                format!(
                    "+{}: {} ({} tokens)",
                    line_num,
                    line.content_str().trim_end(),
                    line.tokens().len()
                )
            }
            LineChange::Deleted { line_num, line, .. } => {
                format!(
                    "-{}: {} ({} tokens)",
                    line_num,
                    line.content_str().trim_end(),
                    line.tokens().len()
                )
            }
            LineChange::Modified {
                old_line_num,
                new_line_num,
                token_changes,
                ..
            } => {
                let changes = token_changes.iter().filter(|tc| tc.is_change()).count();
                format!(
                    "~{}→{}: {} token changes",
                    old_line_num, new_line_num, changes
                )
            }
        }
    }
}

impl<'a> fmt::Display for LineChange<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

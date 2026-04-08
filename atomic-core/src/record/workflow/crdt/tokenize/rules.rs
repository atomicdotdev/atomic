//! Tokenization options and error types.
//!
//! Configuration for how content is split into tokens, including handling
//! of whitespace, comments, and special characters.

use crate::diff::token::TokenizerConfig;
use std::fmt;

// ============================================================================
// TOKENIZE OPTIONS
// ============================================================================

/// Options controlling tokenization behavior.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::TokenizeOptions;
///
/// let options = TokenizeOptions::default()
///     .with_merge_whitespace(true)
///     .with_code_aware(true);
///
/// assert!(options.merge_whitespace());
/// assert!(options.code_aware());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizeOptions {
    /// Whether to merge consecutive whitespace tokens into one.
    merge_whitespace: bool,

    /// Whether to use code-aware tokenization (recognize operators, strings, etc.).
    code_aware: bool,

    /// Whether to include newline tokens in the output.
    include_newlines: bool,

    /// Whether to track byte offsets for each token.
    track_offsets: bool,

    /// Maximum line length before treating as binary.
    max_line_length: usize,
}

impl TokenizeOptions {
    /// Default maximum line length (10KB).
    pub const DEFAULT_MAX_LINE_LENGTH: usize = 10 * 1024;

    /// Creates new options with default settings.
    ///
    /// Default settings:
    /// - `merge_whitespace`: true
    /// - `code_aware`: true
    /// - `include_newlines`: false
    /// - `track_offsets`: true
    /// - `max_line_length`: 10KB
    pub fn new() -> Self {
        Self {
            merge_whitespace: true,
            code_aware: true,
            include_newlines: false,
            track_offsets: true,
            max_line_length: Self::DEFAULT_MAX_LINE_LENGTH,
        }
    }

    /// Sets whether to merge consecutive whitespace tokens.
    ///
    /// When enabled, multiple spaces or tabs are combined into a single
    /// whitespace token.
    pub fn with_merge_whitespace(mut self, merge: bool) -> Self {
        self.merge_whitespace = merge;
        self
    }

    /// Sets whether to use code-aware tokenization.
    ///
    /// When enabled, the tokenizer recognizes multi-character operators,
    /// string literals, numeric literals, and comments.
    pub fn with_code_aware(mut self, aware: bool) -> Self {
        self.code_aware = aware;
        self
    }

    /// Sets whether to include newline tokens.
    pub fn with_include_newlines(mut self, include: bool) -> Self {
        self.include_newlines = include;
        self
    }

    /// Sets whether to track byte offsets.
    pub fn with_track_offsets(mut self, track: bool) -> Self {
        self.track_offsets = track;
        self
    }

    /// Sets the maximum line length before treating content as binary.
    pub fn with_max_line_length(mut self, max: usize) -> Self {
        self.max_line_length = max;
        self
    }

    /// Returns whether whitespace merging is enabled.
    #[inline]
    pub fn merge_whitespace(&self) -> bool {
        self.merge_whitespace
    }

    /// Returns whether code-aware tokenization is enabled.
    #[inline]
    pub fn code_aware(&self) -> bool {
        self.code_aware
    }

    /// Returns whether newline tokens are included.
    #[inline]
    pub fn get_include_newlines(&self) -> bool {
        self.include_newlines
    }

    /// Returns whether offset tracking is enabled.
    #[inline]
    pub fn get_track_offsets(&self) -> bool {
        self.track_offsets
    }

    /// Returns the maximum line length.
    #[inline]
    pub fn get_max_line_length(&self) -> usize {
        self.max_line_length
    }

    /// Converts to a `TokenizerConfig` for the underlying tokenizer.
    pub(crate) fn to_tokenizer_config(&self) -> TokenizerConfig {
        if self.code_aware {
            TokenizerConfig::code()
        } else {
            TokenizerConfig::minimal()
        }
    }
}

impl Default for TokenizeOptions {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// TOKENIZE ERROR
// ============================================================================

/// Errors that can occur during tokenization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenizeError {
    /// Content appears to be binary (contains null bytes or very long lines).
    BinaryContent {
        /// Reason for classification as binary.
        reason: String,
    },

    /// Line exceeds the maximum allowed length.
    LineTooLong {
        /// The line number (0-indexed).
        line_number: usize,
        /// The actual length.
        length: usize,
        /// The maximum allowed length.
        max_length: usize,
    },

    /// Invalid UTF-8 sequence encountered.
    InvalidUtf8 {
        /// The line number where the error occurred.
        line_number: usize,
        /// Byte offset within the line.
        offset: usize,
    },
}

impl fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenizeError::BinaryContent { reason } => {
                write!(f, "binary content detected: {}", reason)
            }
            TokenizeError::LineTooLong {
                line_number,
                length,
                max_length,
            } => {
                write!(
                    f,
                    "line {} is too long ({} bytes, max {})",
                    line_number, length, max_length
                )
            }
            TokenizeError::InvalidUtf8 {
                line_number,
                offset,
            } => {
                write!(
                    f,
                    "invalid UTF-8 at line {}, offset {}",
                    line_number, offset
                )
            }
        }
    }
}

impl std::error::Error for TokenizeError {}

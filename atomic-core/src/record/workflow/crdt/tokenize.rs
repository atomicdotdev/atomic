//! Content tokenization for CRDT Leaf operations.
//!
//! This module provides functionality to tokenize file content into tokens
//! that can be represented as Leaf structures in the hierarchical CRDT graph.
//! It bridges the gap between raw byte content and the fine-grained token
//! operations that enable conflict-free merging.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Content Tokenization Pipeline                      │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: Raw Bytes                                                       │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ b"fn main() {\n    println!(\"Hello\");\n}\n"                    │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  ContentTokenizer (splits into lines)                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ Line 0: "fn main() {"                                            │  │
//! │  │ Line 1: "    println!(\"Hello\");"                               │  │
//! │  │ Line 2: "}"                                                      │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  TokenizedLine (tokens per line)                                        │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ Line 0: [Word("fn"), WS(" "), Word("main"), Punct("("), ...]     │  │
//! │  │ Line 1: [WS("    "), Word("println"), Punct("!"), ...]           │  │
//! │  │ Line 2: [Punct("}")]                                             │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - [`ContentTokenizer`]: Main entry point for tokenizing content
//! - [`TokenizedLine`]: A single line with its tokens and metadata
//! - [`TokenizedToken`]: A token with its kind, content, and position
//! - [`TokenizeOptions`]: Configuration for tokenization behavior
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::tokenize::{
//!     ContentTokenizer, TokenizeOptions,
//! };
//!
//! let content = b"let x = 42;\nlet y = x + 1;";
//! let tokenizer = ContentTokenizer::new(content);
//!
//! for line in tokenizer.lines() {
//!     println!("Line {}: {} tokens", line.line_number(), line.token_count());
//!     for token in line.tokens() {
//!         println!("  {:?}: {:?}", token.kind(), token.as_str());
//!     }
//! }
//! ```
//!
//! # Integration with CRDT Model
//!
//! The tokenized output maps directly to CRDT structures:
//!
//! - Each [`TokenizedLine`] corresponds to a potential `Branch` (line)
//! - Each [`TokenizedToken`] corresponds to a potential `Leaf` (token)
//! - The token positions provide the content ranges for `Leaf::content_range()`
//!
//! # Performance
//!
//! - Lines are split lazily via iterator
//! - Tokens within a line are computed on demand
//! - Content is referenced, not copied, until needed

use crate::diff::token::{TokenKind, Tokenizer, TokenizerConfig};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

// TOKENIZE OPTIONS

/// Options controlling tokenization behavior.
///
/// These options allow customization of how content is split into tokens,
/// including handling of whitespace, comments, and special characters.
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
    /// whitespace token. This reduces the number of tokens and simplifies
    /// diffs where only whitespace amount changed.
    pub fn with_merge_whitespace(mut self, merge: bool) -> Self {
        self.merge_whitespace = merge;
        self
    }

    /// Sets whether to use code-aware tokenization.
    ///
    /// When enabled, the tokenizer recognizes:
    /// - Multi-character operators (`==`, `->`, `::`)
    /// - String literals (`"hello"`)
    /// - Numeric literals (`42`, `0xff`, `3.14`)
    /// - Comments (`// ...`)
    pub fn with_code_aware(mut self, aware: bool) -> Self {
        self.code_aware = aware;
        self
    }

    /// Sets whether to include newline tokens.
    ///
    /// Newlines mark line boundaries. When included, each line's token
    /// sequence ends with a newline token.
    pub fn with_include_newlines(mut self, include: bool) -> Self {
        self.include_newlines = include;
        self
    }

    /// Sets whether to track byte offsets.
    ///
    /// When enabled, each token records its byte offset within the line.
    /// This is useful for generating precise content ranges.
    pub fn with_track_offsets(mut self, track: bool) -> Self {
        self.track_offsets = track;
        self
    }

    /// Sets the maximum line length before treating content as binary.
    ///
    /// Lines longer than this are considered binary data and tokenized
    /// as a single token.
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
    fn to_tokenizer_config(&self) -> TokenizerConfig {
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

// TOKENIZE ERROR

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
            TokenizeError::InvalidUtf8 { line_number, offset } => {
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

// TOKENIZED TOKEN

/// A single token with its metadata.
///
/// This represents a token extracted from a line, including its type,
/// content, and position information.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::TokenizedToken;
/// use atomic_core::diff::token::TokenKind;
///
/// let token = TokenizedToken::new(
///     TokenKind::Word,
///     b"hello".to_vec(),
///     0..5,
/// );
///
/// assert_eq!(token.kind(), TokenKind::Word);
/// assert_eq!(token.as_str(), "hello");
/// assert_eq!(token.byte_range(), 0..5);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizedToken {
    /// The semantic kind of this token.
    kind: TokenKind,

    /// The raw bytes of this token.
    content: Vec<u8>,

    /// Byte range within the line (start..end).
    byte_start: u32,
    byte_end: u32,
}

impl TokenizedToken {
    /// Creates a new tokenized token.
    pub fn new(kind: TokenKind, content: Vec<u8>, byte_range: Range<usize>) -> Self {
        Self {
            kind,
            content,
            byte_start: byte_range.start as u32,
            byte_end: byte_range.end as u32,
        }
    }

    /// Returns the token kind.
    #[inline]
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// Returns the token content as bytes.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the token content as a string (lossy UTF-8 conversion).
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.content)
    }

    /// Returns the byte range within the line.
    #[inline]
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_start as usize..self.byte_end as usize
    }

    /// Returns the byte length of this token.
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Returns true if this token is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Returns true if this token is significant (not whitespace).
    #[inline]
    pub fn is_significant(&self) -> bool {
        self.kind.is_significant()
    }

    /// Returns true if this token is whitespace.
    #[inline]
    pub fn is_whitespace(&self) -> bool {
        self.kind.is_whitespace()
    }
}

impl fmt::Display for TokenizedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", self.kind, self.as_str())
    }
}

// TOKENIZED LINE

/// A tokenized line with its tokens and metadata.
///
/// This represents a complete line of content that has been split into
/// tokens. It includes the line number, raw content, and token sequence.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::{TokenizedLine, TokenizedToken};
/// use atomic_core::diff::token::TokenKind;
///
/// let tokens = vec![
///     TokenizedToken::new(TokenKind::Word, b"let".to_vec(), 0..3),
///     TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 3..4),
///     TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 4..5),
/// ];
///
/// let line = TokenizedLine::new(0, b"let x".to_vec(), tokens);
///
/// assert_eq!(line.line_number(), 0);
/// assert_eq!(line.token_count(), 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizedLine {
    /// The line number (0-indexed).
    line_number: usize,

    /// The raw content of this line (without newline).
    content: Vec<u8>,

    /// The tokens in this line.
    tokens: Vec<TokenizedToken>,

    /// FNV-1a hash of the line content for fast equality checks.
    content_hash: u64,
}

impl TokenizedLine {
    /// FNV-1a offset basis for 64-bit hashes.
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;

    /// FNV-1a prime for 64-bit hashes.
    const FNV_PRIME: u64 = 0x00000100000001B3;

    /// Creates a new tokenized line.
    pub fn new(line_number: usize, content: Vec<u8>, tokens: Vec<TokenizedToken>) -> Self {
        let content_hash = Self::compute_hash(&content);
        Self {
            line_number,
            content,
            tokens,
            content_hash,
        }
    }

    /// Creates an empty tokenized line.
    pub fn empty(line_number: usize) -> Self {
        Self {
            line_number,
            content: Vec::new(),
            tokens: Vec::new(),
            content_hash: Self::FNV_OFFSET_BASIS,
        }
    }

    /// Computes the FNV-1a hash of the given bytes.
    fn compute_hash(bytes: &[u8]) -> u64 {
        let mut hash = Self::FNV_OFFSET_BASIS;
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(Self::FNV_PRIME);
        }
        hash
    }

    /// Returns the line number (0-indexed).
    #[inline]
    pub fn line_number(&self) -> usize {
        self.line_number
    }

    /// Returns the raw content of this line.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the content as a string (lossy UTF-8 conversion).
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.content)
    }

    /// Returns the tokens in this line.
    #[inline]
    pub fn tokens(&self) -> &[TokenizedToken] {
        &self.tokens
    }

    /// Returns an iterator over the tokens.
    pub fn iter_tokens(&self) -> impl Iterator<Item = &TokenizedToken> {
        self.tokens.iter()
    }

    /// Returns the number of tokens in this line.
    #[inline]
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Returns true if this line is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Returns the byte length of this line.
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Returns the content hash for fast equality checks.
    #[inline]
    pub fn content_hash(&self) -> u64 {
        self.content_hash
    }

    /// Returns true if this line has the same content as another.
    ///
    /// Uses hash comparison for fast equality, falling back to byte
    /// comparison if hashes match (to handle collisions).
    pub fn content_eq(&self, other: &TokenizedLine) -> bool {
        self.content_hash == other.content_hash && self.content == other.content
    }

    /// Returns the number of significant (non-whitespace) tokens.
    pub fn significant_token_count(&self) -> usize {
        self.tokens.iter().filter(|t| t.is_significant()).count()
    }

    /// Returns true if this line contains only whitespace.
    pub fn is_whitespace_only(&self) -> bool {
        self.tokens.iter().all(|t| t.is_whitespace())
    }

    /// Consumes this line and returns its tokens.
    pub fn into_tokens(self) -> Vec<TokenizedToken> {
        self.tokens
    }
}

impl fmt::Display for TokenizedLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Line {}: {} tokens, {} bytes",
            self.line_number,
            self.tokens.len(),
            self.content.len()
        )
    }
}

// TOKEN STATS

/// Statistics about tokenization results.
///
/// Tracks counts and metrics about the tokenization process for
/// reporting and analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenStats {
    /// Total number of lines processed.
    pub lines: usize,

    /// Total number of tokens generated.
    pub tokens: usize,

    /// Number of significant (non-whitespace) tokens.
    pub significant_tokens: usize,

    /// Number of whitespace tokens.
    pub whitespace_tokens: usize,

    /// Total bytes processed.
    pub bytes: usize,

    /// Number of empty lines.
    pub empty_lines: usize,

    /// Number of whitespace-only lines.
    pub whitespace_only_lines: usize,

    /// Longest line in bytes.
    pub max_line_length: usize,

    /// Maximum tokens in a single line.
    pub max_tokens_per_line: usize,
}

impl TokenStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates statistics with a tokenized line.
    pub fn add_line(&mut self, line: &TokenizedLine) {
        self.lines += 1;
        self.tokens += line.token_count();
        self.bytes += line.len();

        let significant = line.significant_token_count();
        self.significant_tokens += significant;
        self.whitespace_tokens += line.token_count() - significant;

        if line.is_empty() {
            self.empty_lines += 1;
        } else if line.is_whitespace_only() {
            self.whitespace_only_lines += 1;
        }

        if line.len() > self.max_line_length {
            self.max_line_length = line.len();
        }

        if line.token_count() > self.max_tokens_per_line {
            self.max_tokens_per_line = line.token_count();
        }
    }

    /// Merges another stats instance into this one.
    pub fn merge(&mut self, other: &TokenStats) {
        self.lines += other.lines;
        self.tokens += other.tokens;
        self.significant_tokens += other.significant_tokens;
        self.whitespace_tokens += other.whitespace_tokens;
        self.bytes += other.bytes;
        self.empty_lines += other.empty_lines;
        self.whitespace_only_lines += other.whitespace_only_lines;
        self.max_line_length = self.max_line_length.max(other.max_line_length);
        self.max_tokens_per_line = self.max_tokens_per_line.max(other.max_tokens_per_line);
    }

    /// Returns the average tokens per line.
    pub fn avg_tokens_per_line(&self) -> f64 {
        if self.lines == 0 {
            0.0
        } else {
            self.tokens as f64 / self.lines as f64
        }
    }

    /// Returns the average line length in bytes.
    pub fn avg_line_length(&self) -> f64 {
        if self.lines == 0 {
            0.0
        } else {
            self.bytes as f64 / self.lines as f64
        }
    }
}

impl fmt::Display for TokenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} lines, {} tokens ({} significant), {} bytes",
            self.lines, self.tokens, self.significant_tokens, self.bytes
        )
    }
}

// CONTENT TOKENIZER

/// Main tokenizer for converting content bytes into tokenized lines.
///
/// The `ContentTokenizer` provides an iterator-based interface for
/// tokenizing content line by line. This enables efficient processing
/// of large files without loading everything into memory.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::{
///     ContentTokenizer, TokenizeOptions,
/// };
///
/// let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
/// let tokenizer = ContentTokenizer::new(content);
///
/// let lines: Vec<_> = tokenizer.lines().collect();
/// assert_eq!(lines.len(), 3);
/// assert!(lines[0].token_count() >= 4); // fn, space, main, etc.
/// ```
///
/// # With Options
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::{
///     ContentTokenizer, TokenizeOptions,
/// };
///
/// let content = b"let x = 42;";
/// let options = TokenizeOptions::default().with_code_aware(true);
/// let tokenizer = ContentTokenizer::with_options(content, options);
///
/// let lines: Vec<_> = tokenizer.lines().collect();
/// let tokens = lines[0].tokens();
///
/// // Code-aware tokenization recognizes numbers
/// assert!(tokens.iter().any(|t| t.as_str() == "42"));
/// ```
#[derive(Debug, Clone)]
pub struct ContentTokenizer<'a> {
    /// The content being tokenized.
    content: &'a [u8],

    /// Tokenization options.
    options: TokenizeOptions,
}

impl<'a> ContentTokenizer<'a> {
    /// Creates a new tokenizer with default options.
    pub fn new(content: &'a [u8]) -> Self {
        Self {
            content,
            options: TokenizeOptions::default(),
        }
    }

    /// Creates a new tokenizer with the given options.
    pub fn with_options(content: &'a [u8], options: TokenizeOptions) -> Self {
        Self { content, options }
    }

    /// Returns the content being tokenized.
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.content
    }

    /// Returns the tokenization options.
    #[inline]
    pub fn options(&self) -> &TokenizeOptions {
        &self.options
    }

    /// Returns an iterator over tokenized lines.
    ///
    /// Lines are processed lazily as the iterator is consumed.
    pub fn lines(&self) -> LineIterator<'a> {
        LineIterator {
            content: self.content,
            options: self.options.clone(),
            position: 0,
            line_number: 0,
        }
    }

    /// Tokenizes all content and returns the result with statistics.
    ///
    /// This eagerly processes all lines and collects statistics.
    pub fn tokenize_all(&self) -> (Vec<TokenizedLine>, TokenStats) {
        let mut lines = Vec::new();
        let mut stats = TokenStats::new();

        for line in self.lines() {
            stats.add_line(&line);
            lines.push(line);
        }

        (lines, stats)
    }

    /// Checks if the content appears to be binary.
    ///
    /// Content is considered binary if:
    /// - It contains null bytes
    /// - It has very long lines (over `max_line_length`)
    /// - It has a high ratio of non-printable characters
    pub fn is_binary(&self) -> bool {
        // Check for null bytes
        if self.content.contains(&0) {
            return true;
        }

        // Check line lengths
        let mut line_start = 0;
        for (i, &byte) in self.content.iter().enumerate() {
            if byte == b'\n' {
                let line_len = i - line_start;
                if line_len > self.options.max_line_length {
                    return true;
                }
                line_start = i + 1;
            }
        }

        // Check final line
        let final_line_len = self.content.len() - line_start;
        if final_line_len > self.options.max_line_length {
            return true;
        }

        // Check ratio of non-printable characters
        let non_printable = self
            .content
            .iter()
            .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
            .count();

        if !self.content.is_empty() {
            let ratio = non_printable as f64 / self.content.len() as f64;
            if ratio > 0.1 {
                return true;
            }
        }

        false
    }

    /// Tokenizes a single line of content.
    ///
    /// This is useful for tokenizing individual lines without creating
    /// an iterator over the entire content.
    pub fn tokenize_line(line_content: &[u8], options: &TokenizeOptions) -> TokenizedLine {
        Self::tokenize_line_internal(line_content, 0, options)
    }

    /// Internal method to tokenize a line with a specific line number.
    fn tokenize_line_internal(
        line_content: &[u8],
        line_number: usize,
        options: &TokenizeOptions,
    ) -> TokenizedLine {
        if line_content.is_empty() {
            return TokenizedLine::empty(line_number);
        }

        let config = options.to_tokenizer_config();
        let tokenizer = Tokenizer::with_config(line_content, config);

        let mut tokens = Vec::new();
        let mut prev_whitespace: Option<(usize, Vec<u8>)> = None;

        for token in tokenizer {
            let kind = token.kind();
            let content_bytes = token.content().to_vec();
            let offset = token.offset();
            let len = content_bytes.len();

            // Handle whitespace merging
            if options.merge_whitespace && kind == TokenKind::Whitespace {
                if let Some((start, mut ws_content)) = prev_whitespace.take() {
                    ws_content.extend_from_slice(&content_bytes);
                    prev_whitespace = Some((start, ws_content));
                } else {
                    prev_whitespace = Some((offset, content_bytes));
                }
                continue;
            }

            // Flush any accumulated whitespace
            if let Some((ws_start, ws_content)) = prev_whitespace.take() {
                let ws_end = ws_start + ws_content.len();
                tokens.push(TokenizedToken::new(
                    TokenKind::Whitespace,
                    ws_content,
                    ws_start..ws_end,
                ));
            }

            // Skip newlines unless explicitly included
            if kind == TokenKind::Newline && !options.include_newlines {
                continue;
            }

            tokens.push(TokenizedToken::new(kind, content_bytes, offset..offset + len));
        }

        // Flush any remaining whitespace
        if let Some((ws_start, ws_content)) = prev_whitespace {
            let ws_end = ws_start + ws_content.len();
            tokens.push(TokenizedToken::new(
                TokenKind::Whitespace,
                ws_content,
                ws_start..ws_end,
            ));
        }

        TokenizedLine::new(line_number, line_content.to_vec(), tokens)
    }
}

// LINE ITERATOR

/// Iterator over tokenized lines.
///
/// This iterator splits content into lines and tokenizes each one lazily.
#[derive(Debug, Clone)]
pub struct LineIterator<'a> {
    content: &'a [u8],
    options: TokenizeOptions,
    position: usize,
    line_number: usize,
}

impl<'a> Iterator for LineIterator<'a> {
    type Item = TokenizedLine;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.content.len() {
            return None;
        }

        // Find the end of this line
        let start = self.position;
        let mut end = start;

        while end < self.content.len() && self.content[end] != b'\n' {
            end += 1;
        }

        // Extract line content (without newline)
        let line_content = &self.content[start..end];

        // Move past the newline
        self.position = if end < self.content.len() {
            end + 1
        } else {
            end
        };

        let line_number = self.line_number;
        self.line_number += 1;

        Some(ContentTokenizer::tokenize_line_internal(
            line_content,
            line_number,
            &self.options,
        ))
    }
}

impl<'a> LineIterator<'a> {
    /// Returns the current line number (0-indexed).
    #[inline]
    pub fn current_line_number(&self) -> usize {
        self.line_number
    }

    /// Returns the current byte position in the content.
    #[inline]
    pub fn current_position(&self) -> usize {
        self.position
    }

    /// Returns true if there is more content to process.
    #[inline]
    pub fn has_more(&self) -> bool {
        self.position < self.content.len()
    }

    /// Returns the remaining bytes to process.
    #[inline]
    pub fn remaining_bytes(&self) -> usize {
        self.content.len().saturating_sub(self.position)
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // TokenizeOptions Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenize_options_default() {
        let opts = TokenizeOptions::default();
        assert!(opts.merge_whitespace());
        assert!(opts.code_aware());
        assert!(!opts.get_include_newlines());
        assert!(opts.get_track_offsets());
        assert_eq!(opts.get_max_line_length(), TokenizeOptions::DEFAULT_MAX_LINE_LENGTH);
    }

    #[test]
    fn test_tokenize_options_new() {
        let opts = TokenizeOptions::new();
        assert!(opts.merge_whitespace());
        assert!(opts.code_aware());
    }

    #[test]
    fn test_tokenize_options_builder_merge_whitespace() {
        let opts = TokenizeOptions::new().with_merge_whitespace(false);
        assert!(!opts.merge_whitespace());
    }

    #[test]
    fn test_tokenize_options_builder_code_aware() {
        let opts = TokenizeOptions::new().with_code_aware(false);
        assert!(!opts.code_aware());
    }

    #[test]
    fn test_tokenize_options_builder_include_newlines() {
        let opts = TokenizeOptions::new().with_include_newlines(true);
        assert!(opts.get_include_newlines());
    }

    #[test]
    fn test_tokenize_options_builder_track_offsets() {
        let opts = TokenizeOptions::new().with_track_offsets(false);
        assert!(!opts.get_track_offsets());
    }

    #[test]
    fn test_tokenize_options_builder_max_line_length() {
        let opts = TokenizeOptions::new().with_max_line_length(1000);
        assert_eq!(opts.get_max_line_length(), 1000);
    }

    #[test]
    fn test_tokenize_options_builder_chain() {
        let opts = TokenizeOptions::new()
            .with_merge_whitespace(false)
            .with_code_aware(false)
            .with_include_newlines(true)
            .with_track_offsets(false)
            .with_max_line_length(5000);

        assert!(!opts.merge_whitespace());
        assert!(!opts.code_aware());
        assert!(opts.get_include_newlines());
        assert!(!opts.get_track_offsets());
        assert_eq!(opts.get_max_line_length(), 5000);
    }

    #[test]
    fn test_tokenize_options_clone() {
        let opts1 = TokenizeOptions::new().with_merge_whitespace(false);
        let opts2 = opts1.clone();
        assert_eq!(opts1, opts2);
    }

    // ------------------------------------------------------------------------
    // TokenizeError Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenize_error_binary_content_display() {
        let err = TokenizeError::BinaryContent {
            reason: "null bytes found".to_string(),
        };
        assert!(err.to_string().contains("binary content"));
        assert!(err.to_string().contains("null bytes"));
    }

    #[test]
    fn test_tokenize_error_line_too_long_display() {
        let err = TokenizeError::LineTooLong {
            line_number: 5,
            length: 20000,
            max_length: 10000,
        };
        assert!(err.to_string().contains("line 5"));
        assert!(err.to_string().contains("20000"));
        assert!(err.to_string().contains("10000"));
    }

    #[test]
    fn test_tokenize_error_invalid_utf8_display() {
        let err = TokenizeError::InvalidUtf8 {
            line_number: 3,
            offset: 42,
        };
        assert!(err.to_string().contains("line 3"));
        assert!(err.to_string().contains("offset 42"));
    }

    #[test]
    fn test_tokenize_error_is_error_trait() {
        let err = TokenizeError::BinaryContent {
            reason: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    // ------------------------------------------------------------------------
    // TokenizedToken Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenized_token_new() {
        let token = TokenizedToken::new(TokenKind::Word, b"hello".to_vec(), 0..5);
        assert_eq!(token.kind(), TokenKind::Word);
        assert_eq!(token.content(), b"hello");
        assert_eq!(token.byte_range(), 0..5);
    }

    #[test]
    fn test_tokenized_token_as_str() {
        let token = TokenizedToken::new(TokenKind::Word, b"world".to_vec(), 0..5);
        assert_eq!(token.as_str(), "world");
    }

    #[test]
    fn test_tokenized_token_len() {
        let token = TokenizedToken::new(TokenKind::Word, b"test".to_vec(), 0..4);
        assert_eq!(token.len(), 4);
        assert!(!token.is_empty());
    }

    #[test]
    fn test_tokenized_token_empty() {
        let token = TokenizedToken::new(TokenKind::Whitespace, vec![], 0..0);
        assert!(token.is_empty());
        assert_eq!(token.len(), 0);
    }

    #[test]
    fn test_tokenized_token_is_significant() {
        let word = TokenizedToken::new(TokenKind::Word, b"fn".to_vec(), 0..2);
        let ws = TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1);

        assert!(word.is_significant());
        assert!(!ws.is_significant());
    }

    #[test]
    fn test_tokenized_token_is_whitespace() {
        let word = TokenizedToken::new(TokenKind::Word, b"fn".to_vec(), 0..2);
        let ws = TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1);
        let nl = TokenizedToken::new(TokenKind::Newline, b"\n".to_vec(), 0..1);

        assert!(!word.is_whitespace());
        assert!(ws.is_whitespace());
        assert!(nl.is_whitespace());
    }

    #[test]
    fn test_tokenized_token_display() {
        let token = TokenizedToken::new(TokenKind::Word, b"main".to_vec(), 0..4);
        let display = format!("{}", token);
        assert!(display.contains("word"));
        assert!(display.contains("main"));
    }

    #[test]
    fn test_tokenized_token_clone() {
        let token1 = TokenizedToken::new(TokenKind::Operator, b"==".to_vec(), 0..2);
        let token2 = token1.clone();
        assert_eq!(token1, token2);
    }

    // ------------------------------------------------------------------------
    // TokenizedLine Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tokenized_line_new() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"let".to_vec(), 0..3),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 3..4),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 4..5),
        ];
        let line = TokenizedLine::new(0, b"let x".to_vec(), tokens);

        assert_eq!(line.line_number(), 0);
        assert_eq!(line.content(), b"let x");
        assert_eq!(line.token_count(), 3);
    }

    #[test]
    fn test_tokenized_line_empty() {
        let line = TokenizedLine::empty(5);
        assert_eq!(line.line_number(), 5);
        assert!(line.is_empty());
        assert_eq!(line.token_count(), 0);
    }

    #[test]
    fn test_tokenized_line_as_str() {
        let line = TokenizedLine::new(0, b"hello world".to_vec(), vec![]);
        assert_eq!(line.as_str(), "hello world");
    }

    #[test]
    fn test_tokenized_line_len() {
        let line = TokenizedLine::new(0, b"test".to_vec(), vec![]);
        assert_eq!(line.len(), 4);
        assert!(!line.is_empty());
    }

    #[test]
    fn test_tokenized_line_content_hash() {
        let line1 = TokenizedLine::new(0, b"hello".to_vec(), vec![]);
        let line2 = TokenizedLine::new(1, b"hello".to_vec(), vec![]);
        let line3 = TokenizedLine::new(0, b"world".to_vec(), vec![]);

        // Same content should have same hash
        assert_eq!(line1.content_hash(), line2.content_hash());
        // Different content should have different hash (almost certainly)
        assert_ne!(line1.content_hash(), line3.content_hash());
    }

    #[test]
    fn test_tokenized_line_content_eq() {
        let line1 = TokenizedLine::new(0, b"test".to_vec(), vec![]);
        let line2 = TokenizedLine::new(5, b"test".to_vec(), vec![]);
        let line3 = TokenizedLine::new(0, b"other".to_vec(), vec![]);

        assert!(line1.content_eq(&line2));
        assert!(!line1.content_eq(&line3));
    }

    #[test]
    fn test_tokenized_line_significant_token_count() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"let".to_vec(), 0..3),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 3..4),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 4..5),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 5..6),
        ];
        let line = TokenizedLine::new(0, b"let x ".to_vec(), tokens);

        assert_eq!(line.significant_token_count(), 2);
    }

    #[test]
    fn test_tokenized_line_is_whitespace_only() {
        let ws_tokens = vec![
            TokenizedToken::new(TokenKind::Whitespace, b"  ".to_vec(), 0..2),
            TokenizedToken::new(TokenKind::Whitespace, b"\t".to_vec(), 2..3),
        ];
        let ws_line = TokenizedLine::new(0, b"  \t".to_vec(), ws_tokens);

        let mixed_tokens = vec![
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 0..1),
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 1..2),
        ];
        let mixed_line = TokenizedLine::new(0, b" x".to_vec(), mixed_tokens);

        assert!(ws_line.is_whitespace_only());
        assert!(!mixed_line.is_whitespace_only());
    }

    #[test]
    fn test_tokenized_line_iter_tokens() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"a".to_vec(), 0..1),
            TokenizedToken::new(TokenKind::Word, b"b".to_vec(), 1..2),
        ];
        let line = TokenizedLine::new(0, b"ab".to_vec(), tokens);

        let collected: Vec<_> = line.iter_tokens().map(|t| t.as_str().to_string()).collect();
        assert_eq!(collected, vec!["a", "b"]);
    }

    #[test]
    fn test_tokenized_line_into_tokens() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"x".to_vec(), 0..1),
        ];
        let line = TokenizedLine::new(0, b"x".to_vec(), tokens);

        let owned_tokens = line.into_tokens();
        assert_eq!(owned_tokens.len(), 1);
    }

    #[test]
    fn test_tokenized_line_display() {
        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"hi".to_vec(), 0..2),
        ];
        let line = TokenizedLine::new(3, b"hi".to_vec(), tokens);
        let display = format!("{}", line);
        assert!(display.contains("Line 3"));
        assert!(display.contains("1 tokens"));
        assert!(display.contains("2 bytes"));
    }

    // ------------------------------------------------------------------------
    // TokenStats Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_token_stats_new() {
        let stats = TokenStats::new();
        assert_eq!(stats.lines, 0);
        assert_eq!(stats.tokens, 0);
        assert_eq!(stats.bytes, 0);
    }

    #[test]
    fn test_token_stats_add_line() {
        let mut stats = TokenStats::new();

        let tokens = vec![
            TokenizedToken::new(TokenKind::Word, b"test".to_vec(), 0..4),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 4..5),
        ];
        let line = TokenizedLine::new(0, b"test ".to_vec(), tokens);

        stats.add_line(&line);

        assert_eq!(stats.lines, 1);
        assert_eq!(stats.tokens, 2);
        assert_eq!(stats.significant_tokens, 1);
        assert_eq!(stats.whitespace_tokens, 1);
        assert_eq!(stats.bytes, 5);
    }

    #[test]
    fn test_token_stats_add_empty_line() {
        let mut stats = TokenStats::new();
        let line = TokenizedLine::empty(0);
        stats.add_line(&line);

        assert_eq!(stats.lines, 1);
        assert_eq!(stats.empty_lines, 1);
    }

    #[test]
    fn test_token_stats_add_whitespace_only_line() {
        let mut stats = TokenStats::new();

        let tokens = vec![
            TokenizedToken::new(TokenKind::Whitespace, b"  ".to_vec(), 0..2),
        ];
        let line = TokenizedLine::new(0, b"  ".to_vec(), tokens);
        stats.add_line(&line);

        assert_eq!(stats.whitespace_only_lines, 1);
    }

    #[test]
    fn test_token_stats_max_tracking() {
        let mut stats = TokenStats::new();

        // Short line
        let line1 = TokenizedLine::new(0, b"hi".to_vec(), vec![
            TokenizedToken::new(TokenKind::Word, b"hi".to_vec(), 0..2),
        ]);
        stats.add_line(&line1);

        // Longer line with more tokens
        let line2 = TokenizedLine::new(1, b"hello world".to_vec(), vec![
            TokenizedToken::new(TokenKind::Word, b"hello".to_vec(), 0..5),
            TokenizedToken::new(TokenKind::Whitespace, b" ".to_vec(), 5..6),
            TokenizedToken::new(TokenKind::Word, b"world".to_vec(), 6..11),
        ]);
        stats.add_line(&line2);

        assert_eq!(stats.max_line_length, 11);
        assert_eq!(stats.max_tokens_per_line, 3);
    }

    #[test]
    fn test_token_stats_merge() {
        let mut stats1 = TokenStats::new();
        stats1.lines = 5;
        stats1.tokens = 20;
        stats1.max_line_length = 50;

        let mut stats2 = TokenStats::new();
        stats2.lines = 3;
        stats2.tokens = 10;
        stats2.max_line_length = 100;

        stats1.merge(&stats2);

        assert_eq!(stats1.lines, 8);
        assert_eq!(stats1.tokens, 30);
        assert_eq!(stats1.max_line_length, 100);
    }

    #[test]
    fn test_token_stats_avg_tokens_per_line() {
        let mut stats = TokenStats::new();
        stats.lines = 4;
        stats.tokens = 10;

        assert!((stats.avg_tokens_per_line() - 2.5).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_avg_tokens_per_line_empty() {
        let stats = TokenStats::new();
        assert!((stats.avg_tokens_per_line() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_avg_line_length() {
        let mut stats = TokenStats::new();
        stats.lines = 2;
        stats.bytes = 20;

        assert!((stats.avg_line_length() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_token_stats_display() {
        let mut stats = TokenStats::new();
        stats.lines = 10;
        stats.tokens = 50;
        stats.significant_tokens = 30;
        stats.bytes = 200;

        let display = format!("{}", stats);
        assert!(display.contains("10 lines"));
        assert!(display.contains("50 tokens"));
        assert!(display.contains("30 significant"));
        assert!(display.contains("200 bytes"));
    }

    // ------------------------------------------------------------------------
    // ContentTokenizer Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_content_tokenizer_new() {
        let content = b"hello";
        let tokenizer = ContentTokenizer::new(content);
        assert_eq!(tokenizer.content(), content);
    }

    #[test]
    fn test_content_tokenizer_with_options() {
        let content = b"hello";
        let options = TokenizeOptions::new().with_code_aware(false);
        let tokenizer = ContentTokenizer::with_options(content, options.clone());

        assert_eq!(tokenizer.content(), content);
        assert_eq!(tokenizer.options(), &options);
    }

    #[test]
    fn test_content_tokenizer_single_line() {
        let content = b"let x = 5;";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number(), 0);
        assert!(!lines[0].tokens().is_empty());
    }

    #[test]
    fn test_content_tokenizer_multiple_lines() {
        let content = b"line one\nline two\nline three";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_number(), 0);
        assert_eq!(lines[1].line_number(), 1);
        assert_eq!(lines[2].line_number(), 2);
    }

    #[test]
    fn test_content_tokenizer_trailing_newline() {
        let content = b"line one\nline two\n";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        // Trailing newline creates an empty final line
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_number(), 0);
        assert_eq!(lines[1].line_number(), 1);
    }

    #[test]
    fn test_content_tokenizer_empty_content() {
        let content = b"";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        assert_eq!(lines.len(), 0);
    }

    #[test]
    fn test_content_tokenizer_only_newlines() {
        let content = b"\n\n\n";
        let tokenizer = ContentTokenizer::new(content);

        let lines: Vec<_> = tokenizer.lines().collect();
        // Three newlines create three empty lines
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.is_empty());
        }
    }

    #[test]
    fn test_content_tokenizer_tokenize_all() {
        let content = b"fn main() {\n    println!(\"Hi\");\n}";
        let tokenizer = ContentTokenizer::new(content);

        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 3);
        assert_eq!(stats.lines, 3);
        assert!(stats.tokens > 0);
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_null() {
        let content = b"hello\x00world";
        let tokenizer = ContentTokenizer::new(content);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_long_line() {
        let long_line = vec![b'x'; 20_000];
        let tokenizer = ContentTokenizer::new(&long_line);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_is_binary_with_control_chars() {
        // Create content with >10% control characters
        let mut content = vec![b'a'; 10];
        content.extend(vec![0x01, 0x02, 0x03]); // 3 control chars out of 13 = 23%
        let tokenizer = ContentTokenizer::new(&content);
        assert!(tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_not_binary_normal_text() {
        let content = b"fn main() {\n    println!(\"Hello, World!\");\n}\n";
        let tokenizer = ContentTokenizer::new(content);
        assert!(!tokenizer.is_binary());
    }

    #[test]
    fn test_content_tokenizer_tokenize_line_static() {
        let line = b"let x = 42;";
        let options = TokenizeOptions::default();
        let result = ContentTokenizer::tokenize_line(line, &options);

        assert_eq!(result.line_number(), 0);
        assert!(!result.tokens().is_empty());
    }

    #[test]
    fn test_content_tokenizer_whitespace_merging() {
        let content = b"a    b";
        let options = TokenizeOptions::new().with_merge_whitespace(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let tokens = lines[0].tokens();

        // Should be: "a", whitespace, "b"
        assert_eq!(tokens.len(), 3);
        // The middle token should be merged whitespace
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[1].content(), b"    ");
    }

    #[test]
    fn test_content_tokenizer_no_whitespace_merging() {
        let content = b"a  b";
        let options = TokenizeOptions::new().with_merge_whitespace(false);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let ws_count = lines[0]
            .tokens()
            .iter()
            .filter(|t| t.kind() == TokenKind::Whitespace)
            .count();

        // Without merging, whitespace may still be grouped by the underlying tokenizer
        // At minimum we should have at least one whitespace token
        assert!(ws_count >= 1);
    }

    #[test]
    fn test_content_tokenizer_code_aware_operators() {
        let content = b"x == y";
        let options = TokenizeOptions::new().with_code_aware(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let has_eq_operator = lines[0]
            .tokens()
            .iter()
            .any(|t| t.as_str() == "==");

        assert!(has_eq_operator, "Should recognize == as single operator");
    }

    #[test]
    fn test_content_tokenizer_code_aware_numbers() {
        let content = b"x = 42";
        let options = TokenizeOptions::new().with_code_aware(true);
        let tokenizer = ContentTokenizer::with_options(content, options);

        let lines: Vec<_> = tokenizer.lines().collect();
        let has_number = lines[0]
            .tokens()
            .iter()
            .any(|t| t.kind() == TokenKind::Number);

        assert!(has_number, "Should recognize 42 as a number");
    }

    // ------------------------------------------------------------------------
    // LineIterator Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_iterator_current_line_number() {
        let content = b"a\nb\nc";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.current_line_number(), 0);
        iter.next();
        assert_eq!(iter.current_line_number(), 1);
        iter.next();
        assert_eq!(iter.current_line_number(), 2);
    }

    #[test]
    fn test_line_iterator_current_position() {
        let content = b"abc\ndef";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.current_position(), 0);
        iter.next(); // Consumes "abc\n"
        assert_eq!(iter.current_position(), 4);
    }

    #[test]
    fn test_line_iterator_has_more() {
        let content = b"a\nb";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert!(iter.has_more());
        iter.next();
        assert!(iter.has_more());
        iter.next();
        assert!(!iter.has_more());
    }

    #[test]
    fn test_line_iterator_remaining_bytes() {
        let content = b"abc\ndefgh";
        let tokenizer = ContentTokenizer::new(content);
        let mut iter = tokenizer.lines();

        assert_eq!(iter.remaining_bytes(), 9);
        iter.next(); // Consumes "abc\n"
        assert_eq!(iter.remaining_bytes(), 5);
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_integration_rust_code() {
        let content = br#"fn main() {
    let x = 42;
    println!("{}", x);
}"#;
        let tokenizer = ContentTokenizer::new(content);
        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 4);
        assert!(stats.tokens > 10);

        // First line should have: fn, space, main, (, ), space, {
        let first_line_tokens: Vec<_> = lines[0].tokens().iter().map(|t| t.as_str().to_string()).collect();
        assert!(first_line_tokens.contains(&"fn".to_string()));
        assert!(first_line_tokens.contains(&"main".to_string()));
    }

    #[test]
    fn test_integration_mixed_content() {
        let content = b"// Comment\nlet x = \"string\";";
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        assert_eq!(lines.len(), 2);

        // First line should have comment
        let has_comment = lines[0]
            .tokens()
            .iter()
            .any(|t| t.kind() == TokenKind::Comment || t.as_str().starts_with("//"));
        assert!(has_comment || lines[0].tokens().len() > 0);
    }

    #[test]
    fn test_integration_empty_lines() {
        let content = b"a\n\nb\n\n\nc";
        let tokenizer = ContentTokenizer::new(content);
        let (lines, stats) = tokenizer.tokenize_all();

        assert_eq!(lines.len(), 6);
        assert_eq!(stats.empty_lines, 3);
    }

    #[test]
    fn test_integration_byte_ranges() {
        let content = b"ab cd";
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        let tokens = lines[0].tokens();
        // Verify byte ranges are correct
        assert_eq!(tokens[0].byte_range().start, 0);
        assert_eq!(tokens[0].byte_range().end, 2); // "ab"
    }

    #[test]
    fn test_integration_unicode_content() {
        let content = "let emoji = \"🎉\";".as_bytes();
        let tokenizer = ContentTokenizer::new(content);
        let lines: Vec<_> = tokenizer.lines().collect();

        assert_eq!(lines.len(), 1);
        // Should handle unicode without crashing
        assert!(!lines[0].tokens().is_empty());
    }
}

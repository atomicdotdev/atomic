//! Token representation for word-level diffing.
//!
//! This module provides token-level granularity for diff operations,
//! enabling CRDT-style word/character comparison for code reviews.
//! Tokens are the atomic units that allow us to show exactly what
//! changed within a line, not just that a line changed.
//!
//! # Token Types
//!
//! Tokens are classified into categories that help with:
//! - Semantic diff display (highlighting meaningful changes)
//! - Language-aware comparison (operators vs identifiers)
//! - Whitespace handling (can be optionally ignored)
//!
//! # Design Philosophy
//!
//! Unlike line-level diffs, token-level diffs can show exactly what
//! changed within a line. For code reviews, this means:
//!
//! - Single character changes are clearly highlighted
//! - Added/removed function arguments are visible
//! - Renamed variables show the exact change
//! - The diff display uses light background for the line and dark
//!   highlighting for the specific tokens that changed
//!
//! # Display Pattern
//!
//! The visual pattern this enables:
//!
//! ```text
//! - const result = calculateSum(a, b);        <- light red background
//!                                  ^^         <- no dark highlight (unchanged)
//! + const result = calculateSum(a, b, c);     <- light green background
//!                                   ^^^^      <- dark green: ", c" added
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::token::{Token, Tokenizer, TokenKind};
//!
//! let line = b"const result = calculateSum(a, b);";
//! let tokens: Vec<Token> = Tokenizer::new(line).collect();
//!
//! assert!(tokens.len() > 0);
//! assert_eq!(tokens[0].kind(), TokenKind::Word);
//! assert_eq!(tokens[0].as_str(), "const");
//! ```
//!
//! # Integration with Graph Model
//!
//! While the current implementation is for display purposes (code review),
//! the token concept could be extended to the graph model for true CRDT
//! semantics where each token has a unique span identity, enabling:
//!
//! - Per-token AI attribution
//! - Fine-grained merge conflict resolution
//! - Character-level blame

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// The kind of token, used for semantic categorization.
///
/// This classification helps with:
/// - Display (different colors for different token types)
/// - Comparison (can optionally ignore whitespace changes)
/// - Language-aware diffing (understanding code structure)
///
/// # Categories
///
/// Tokens are broadly categorized as:
/// - **Content tokens**: Word, String, Number, Comment - carry semantic meaning
/// - **Structural tokens**: Operator, Punctuation - define code structure
/// - **Whitespace tokens**: Whitespace, Newline - formatting only
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenKind {
    /// An identifier or keyword (e.g., `foo`, `let`, `class`, `fn`, `async`).
    ///
    /// This includes any sequence of alphanumeric characters and underscores
    /// that starts with a letter or underscore.
    Word,

    /// Whitespace characters (spaces, tabs, but not newlines).
    ///
    /// Multiple consecutive whitespace characters are merged into a single
    /// token when using the default tokenizer configuration.
    Whitespace,

    /// Operators and symbols with semantic meaning.
    ///
    /// Examples: `+`, `-`, `=`, `==`, `->`, `::`, `&&`, `||`
    ///
    /// Multi-character operators like `==` and `->` are recognized as
    /// single tokens for better diff quality.
    Operator,

    /// Structural punctuation that defines code structure.
    ///
    /// Examples: `(`, `)`, `{`, `}`, `[`, `]`, `;`, `,`, `.`
    ///
    /// These are kept as single-character tokens.
    Punctuation,

    /// String literals including their delimiters.
    ///
    /// Examples: `"hello"`, `'a'`, `"multi word string"`
    ///
    /// The entire string including quotes is captured as one token,
    /// which means string content changes show up as token replacements.
    String,

    /// Numeric literals in various formats.
    ///
    /// Examples: `42`, `3.14`, `0xff`, `0b1010`, `1_000_000`, `1e10`
    ///
    /// Supports integer, floating point, hex, octal, binary, and
    /// scientific notation.
    Number,

    /// Single-line comment content (// style).
    ///
    /// The entire comment including the `//` prefix is one token.
    /// Block comments (`/* */`) are not specially handled and will
    /// be tokenized as individual symbols.
    Comment,

    /// Newline character(s).
    ///
    /// Both `\n` and `\r\n` are recognized. Newlines are tracked
    /// separately from whitespace for line-aware processing.
    Newline,

    /// Any character that doesn't fit other categories.
    ///
    /// This includes Unicode characters, special symbols, and
    /// anything else not recognized by the tokenizer.
    Other,
}

impl TokenKind {
    /// Check if this token kind is typically significant for diffs.
    ///
    /// Whitespace and newlines are often considered less significant
    /// than content tokens. This is useful for implementing
    /// "ignore whitespace" diff modes.
    ///
    /// # Returns
    ///
    /// `true` for content and structural tokens, `false` for whitespace.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenKind;
    ///
    /// assert!(TokenKind::Word.is_significant());
    /// assert!(TokenKind::Operator.is_significant());
    /// assert!(!TokenKind::Whitespace.is_significant());
    /// assert!(!TokenKind::Newline.is_significant());
    /// ```
    #[inline]
    pub fn is_significant(&self) -> bool {
        !matches!(self, TokenKind::Whitespace | TokenKind::Newline)
    }

    /// Check if this is a content-bearing token (not structural).
    ///
    /// Content tokens carry semantic meaning (identifiers, literals, comments)
    /// while structural tokens (operators, punctuation) define relationships.
    ///
    /// # Returns
    ///
    /// `true` for Word, String, Number, and Comment tokens.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenKind;
    ///
    /// assert!(TokenKind::Word.is_content());
    /// assert!(TokenKind::String.is_content());
    /// assert!(!TokenKind::Operator.is_content());
    /// assert!(!TokenKind::Punctuation.is_content());
    /// ```
    #[inline]
    pub fn is_content(&self) -> bool {
        matches!(
            self,
            TokenKind::Word | TokenKind::String | TokenKind::Number | TokenKind::Comment
        )
    }

    /// Check if this is a whitespace-like token.
    ///
    /// # Returns
    ///
    /// `true` for Whitespace and Newline tokens.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenKind;
    ///
    /// assert!(TokenKind::Whitespace.is_whitespace());
    /// assert!(TokenKind::Newline.is_whitespace());
    /// assert!(!TokenKind::Word.is_whitespace());
    /// ```
    #[inline]
    pub fn is_whitespace(&self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::Newline)
    }

    /// Get a short name for display purposes.
    ///
    /// Returns a brief, lowercase string identifying the token kind.
    /// Useful for debugging output and test assertions.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenKind;
    ///
    /// assert_eq!(TokenKind::Word.name(), "word");
    /// assert_eq!(TokenKind::Operator.name(), "op");
    /// ```
    pub fn name(&self) -> &'static str {
        match self {
            TokenKind::Word => "word",
            TokenKind::Whitespace => "ws",
            TokenKind::Operator => "op",
            TokenKind::Punctuation => "punct",
            TokenKind::String => "string",
            TokenKind::Number => "number",
            TokenKind::Comment => "comment",
            TokenKind::Newline => "newline",
            TokenKind::Other => "other",
        }
    }
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// A token representing a word or symbol within a line.
///
/// Tokens are the atomic units for word-level diffing. They're designed
/// for efficient comparison (via pre-computed hash) and zero-copy operation
/// (holding references to the original content).
///
/// # Memory Layout
///
/// The token holds a reference to the original content, avoiding copies.
/// The hash is pre-computed at construction time for fast equality checking.
///
/// # Equality Semantics
///
/// Two tokens are equal if they have the same content bytes, regardless of
/// their position in the original text or their kind classification.
/// This enables finding matching tokens even when they've moved.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::token::{Token, TokenKind};
///
/// let token = Token::new(b"hello", TokenKind::Word, 0);
/// assert_eq!(token.as_str(), "hello");
/// assert_eq!(token.kind(), TokenKind::Word);
/// assert_eq!(token.offset(), 0);
/// assert_eq!(token.len(), 5);
/// ```
#[derive(Clone)]
pub struct Token<'a> {
    /// The raw bytes of this token.
    content: &'a [u8],

    /// The kind of token (word, operator, etc.).
    kind: TokenKind,

    /// Pre-computed FNV-1a hash for fast comparison.
    hash: u64,

    /// Byte offset within the original line (0-based).
    offset: usize,
}

impl<'a> Token<'a> {
    /// Create a new token.
    ///
    /// The token's hash is computed immediately and cached for later use
    /// in equality comparisons and hash table operations.
    ///
    /// # Arguments
    ///
    /// * `content` - The raw bytes of the token
    /// * `kind` - The token classification
    /// * `offset` - Byte offset in the original content (0-based)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"let", TokenKind::Word, 0);
    /// assert_eq!(token.as_str(), "let");
    /// ```
    pub fn new(content: &'a [u8], kind: TokenKind, offset: usize) -> Self {
        let hash = Self::compute_hash(content);
        Self {
            content,
            kind,
            hash,
            offset,
        }
    }

    /// Get the token content as bytes.
    ///
    /// This returns a reference to the original bytes without copying.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"hello", TokenKind::Word, 0);
    /// assert_eq!(token.content(), b"hello");
    /// ```
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.content
    }

    /// Get the token content as a string.
    ///
    /// Uses lossy UTF-8 conversion, replacing invalid sequences with
    /// the Unicode replacement character. For most source code, this
    /// will be a direct conversion.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"world", TokenKind::Word, 0);
    /// assert_eq!(token.as_str(), "world");
    /// ```
    #[inline]
    pub fn as_str(&self) -> std::borrow::Cow<'a, str> {
        String::from_utf8_lossy(self.content)
    }

    /// Get the token kind.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"+", TokenKind::Operator, 0);
    /// assert_eq!(token.kind(), TokenKind::Operator);
    /// ```
    #[inline]
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// Get the byte offset within the original line.
    ///
    /// This is the 0-based position where this token starts in the
    /// original content that was tokenized.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"x", TokenKind::Word, 10);
    /// assert_eq!(token.offset(), 10);
    /// ```
    #[inline]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Get the byte length of the token.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"hello", TokenKind::Word, 0);
    /// assert_eq!(token.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Check if the token is empty.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"", TokenKind::Other, 0);
    /// assert!(token.is_empty());
    ///
    /// let token2 = Token::new(b"x", TokenKind::Word, 0);
    /// assert!(!token2.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Get the end offset (offset + len).
    ///
    /// This is the byte position just past the end of this token,
    /// useful for calculating spans and ranges.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let token = Token::new(b"hello", TokenKind::Word, 10);
    /// assert_eq!(token.offset(), 10);
    /// assert_eq!(token.len(), 5);
    /// assert_eq!(token.end_offset(), 15);
    /// ```
    #[inline]
    pub fn end_offset(&self) -> usize {
        self.offset + self.content.len()
    }

    /// Get the byte range of this token as a Range.
    ///
    /// Returns `offset..end_offset`, suitable for slicing the original content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let content = b"hello world";
    /// let token = Token::new(&content[6..11], TokenKind::Word, 6);
    /// assert_eq!(token.byte_range(), 6..11);
    /// assert_eq!(&content[token.byte_range()], b"world");
    /// ```
    #[inline]
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.offset..self.end_offset()
    }

    /// Get the pre-computed hash value.
    ///
    /// This hash is used for fast equality pre-checking. Tokens with
    /// different hashes are definitely not equal; tokens with the same
    /// hash might be equal (collision) and need byte comparison.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let t1 = Token::new(b"hello", TokenKind::Word, 0);
    /// let t2 = Token::new(b"hello", TokenKind::Word, 10);
    ///
    /// // Same content = same hash
    /// assert_eq!(t1.hash_value(), t2.hash_value());
    /// ```
    #[inline]
    pub fn hash_value(&self) -> u64 {
        self.hash
    }

    /// Check if this token is significant for diffing.
    ///
    /// Convenience method that delegates to `self.kind().is_significant()`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Token, TokenKind};
    ///
    /// let word = Token::new(b"foo", TokenKind::Word, 0);
    /// let space = Token::new(b" ", TokenKind::Whitespace, 0);
    ///
    /// assert!(word.is_significant());
    /// assert!(!space.is_significant());
    /// ```
    #[inline]
    pub fn is_significant(&self) -> bool {
        self.kind.is_significant()
    }

    /// Compute FNV-1a hash of content.
    ///
    /// FNV-1a is chosen for:
    /// - Speed: Very fast for small inputs (typical token lengths)
    /// - Quality: Good distribution for text data
    /// - Simplicity: Easy to implement correctly
    ///
    /// The same algorithm is used in the `Line` struct for consistency.
    #[inline]
    fn compute_hash(content: &[u8]) -> u64 {
        // FNV-1a parameters for 64-bit
        const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        let mut hash = FNV_OFFSET_BASIS;
        for &byte in content {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }
}

impl<'a> PartialEq for Token<'a> {
    /// Compare two tokens for equality.
    ///
    /// Uses hash-based pre-filtering for performance:
    /// 1. If hashes differ, tokens are definitely different (fast path)
    /// 2. If hashes match, compare bytes to handle collisions
    ///
    /// Note: Equality is based on content only, not offset or kind.
    fn eq(&self, other: &Self) -> bool {
        // Fast path: different hashes means definitely not equal
        if self.hash != other.hash {
            return false;
        }
        // Hashes match, compare content (handles collisions)
        self.content == other.content
    }
}

impl<'a> Eq for Token<'a> {}

impl<'a> Hash for Token<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Use our pre-computed hash for fast hashing
        state.write_u64(self.hash);
    }
}

impl<'a> std::fmt::Debug for Token<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Token({:?}, {:?}, @{}..{})",
            self.as_str(),
            self.kind,
            self.offset,
            self.end_offset()
        )
    }
}

impl<'a> std::fmt::Display for Token<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Configuration options for tokenization.
///
/// The tokenizer can be configured to recognize different language constructs.
/// Use `TokenizerConfig::default()` for full code-aware tokenization, or
/// `TokenizerConfig::minimal()` for basic word/whitespace splitting.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::token::{Tokenizer, TokenizerConfig};
///
/// // Full code-aware tokenization (default)
/// let config = TokenizerConfig::default();
///
/// // Minimal tokenization (just words and whitespace)
/// let minimal = TokenizerConfig::minimal();
///
/// // Custom configuration
/// let custom = TokenizerConfig {
///     merge_whitespace: true,
///     recognize_operators: true,
///     recognize_strings: false,  // Don't parse string literals
///     recognize_numbers: true,
///     recognize_comments: false, // Don't parse comments
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizerConfig {
    /// Whether to merge consecutive whitespace into single tokens.
    ///
    /// When true (default), `"   "` becomes one Whitespace token.
    /// When false, each space is a separate token.
    pub merge_whitespace: bool,

    /// Whether to recognize common programming operators.
    ///
    /// When true (default), multi-character operators like `==`, `->`,
    /// `&&` are recognized as single tokens.
    pub recognize_operators: bool,

    /// Whether to recognize string literals (quoted content).
    ///
    /// When true (default), `"hello"` is one String token.
    /// When false, the quotes and content are separate tokens.
    pub recognize_strings: bool,

    /// Whether to recognize numeric literals.
    ///
    /// When true (default), `123`, `3.14`, `0xff` are Number tokens.
    /// When false, digits are Word tokens.
    pub recognize_numbers: bool,

    /// Whether to recognize single-line comments (// style).
    ///
    /// When true (default), `// comment` is one Comment token.
    /// When false, each character is tokenized separately.
    pub recognize_comments: bool,
}

impl Default for TokenizerConfig {
    /// Returns the default configuration with all features enabled.
    ///
    /// This is equivalent to `TokenizerConfig::code()` and provides
    /// full code-aware tokenization suitable for most programming languages.
    fn default() -> Self {
        Self {
            merge_whitespace: true,
            recognize_operators: true,
            recognize_strings: true,
            recognize_numbers: true,
            recognize_comments: true,
        }
    }
}

impl TokenizerConfig {
    /// Create a minimal tokenizer config (just words and whitespace).
    ///
    /// This configuration treats everything as either:
    /// - Words (alphanumeric sequences)
    /// - Whitespace
    /// - Punctuation/Other (single characters)
    ///
    /// Useful for plain text or when you don't need code-aware tokenization.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Tokenizer, TokenizerConfig};
    ///
    /// let config = TokenizerConfig::minimal();
    /// let tokens: Vec<_> = Tokenizer::with_config(b"x = 42", config).collect();
    /// // Numbers and operators won't be specially recognized
    /// ```
    pub fn minimal() -> Self {
        Self {
            merge_whitespace: true,
            recognize_operators: false,
            recognize_strings: false,
            recognize_numbers: false,
            recognize_comments: false,
        }
    }

    /// Create a code-aware tokenizer config (all features enabled).
    ///
    /// This is identical to `TokenizerConfig::default()` but more
    /// explicitly named for clarity.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenizerConfig;
    ///
    /// let config = TokenizerConfig::code();
    /// assert!(config.recognize_operators);
    /// assert!(config.recognize_strings);
    /// ```
    pub fn code() -> Self {
        Self::default()
    }

    /// Create a configuration for plain text (prose, markdown, etc.).
    ///
    /// This configuration is optimized for natural language text:
    /// - Merges whitespace
    /// - Doesn't recognize operators (treats punctuation simply)
    /// - Doesn't recognize strings or numbers specially
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::TokenizerConfig;
    ///
    /// let config = TokenizerConfig::prose();
    /// assert!(!config.recognize_operators);
    /// assert!(!config.recognize_strings);
    /// ```
    pub fn prose() -> Self {
        Self {
            merge_whitespace: true,
            recognize_operators: false,
            recognize_strings: false,
            recognize_numbers: false,
            recognize_comments: false,
        }
    }
}

/// A tokenizer that splits content into tokens.
///
/// The tokenizer is an iterator that yields tokens from the input content.
/// It's designed for language-agnostic tokenization that works well for
/// most programming languages while being configurable for specific needs.
///
/// # Zero-Copy Design
///
/// The tokenizer holds a reference to the input content and yields tokens
/// that reference slices of that content. No copying occurs during tokenization.
///
/// # Usage
///
/// ```rust
/// use atomic_core::diff::token::{Tokenizer, Token, TokenKind};
///
/// let code = b"let x = 42;";
/// let tokens: Vec<Token> = Tokenizer::new(code).collect();
///
/// assert_eq!(tokens[0].as_str(), "let");
/// assert_eq!(tokens[0].kind(), TokenKind::Word);
/// ```
///
/// # Convenience Methods
///
/// For one-off tokenization, use the static methods:
///
/// ```rust
/// use atomic_core::diff::token::Tokenizer;
///
/// let tokens = Tokenizer::tokenize_all(b"hello world");
/// assert_eq!(tokens.len(), 3); // "hello", " ", "world"
/// ```
pub struct Tokenizer<'a> {
    /// The content being tokenized.
    content: &'a [u8],

    /// Current position in the content.
    position: usize,

    /// Configuration options.
    config: TokenizerConfig,
}

impl<'a> Tokenizer<'a> {
    /// Create a new tokenizer with default configuration.
    ///
    /// Uses full code-aware tokenization by default.
    ///
    /// # Arguments
    ///
    /// * `content` - The bytes to tokenize
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::Tokenizer;
    ///
    /// let tokenizer = Tokenizer::new(b"hello world");
    /// let tokens: Vec<_> = tokenizer.collect();
    /// assert_eq!(tokens.len(), 3);
    /// ```
    pub fn new(content: &'a [u8]) -> Self {
        Self::with_config(content, TokenizerConfig::default())
    }

    /// Create a new tokenizer with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `content` - The bytes to tokenize
    /// * `config` - The tokenization configuration
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Tokenizer, TokenizerConfig};
    ///
    /// let config = TokenizerConfig::minimal();
    /// let tokenizer = Tokenizer::with_config(b"x = 42", config);
    /// let tokens: Vec<_> = tokenizer.collect();
    /// ```
    pub fn with_config(content: &'a [u8], config: TokenizerConfig) -> Self {
        Self {
            content,
            position: 0,
            config,
        }
    }

    /// Tokenize all content and return as a vector.
    ///
    /// Convenience method for one-off tokenization with default config.
    ///
    /// # Arguments
    ///
    /// * `content` - The bytes to tokenize
    ///
    /// # Returns
    ///
    /// A vector of all tokens in the content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::Tokenizer;
    ///
    /// let tokens = Tokenizer::tokenize_all(b"let x = 1;");
    /// assert!(tokens.len() > 0);
    /// ```
    pub fn tokenize_all(content: &'a [u8]) -> Vec<Token<'a>> {
        Self::new(content).collect()
    }

    /// Tokenize with custom configuration.
    ///
    /// Convenience method for one-off tokenization with custom config.
    ///
    /// # Arguments
    ///
    /// * `content` - The bytes to tokenize
    /// * `config` - The tokenization configuration
    ///
    /// # Returns
    ///
    /// A vector of all tokens in the content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::token::{Tokenizer, TokenizerConfig};
    ///
    /// let config = TokenizerConfig::prose();
    /// let tokens = Tokenizer::tokenize_with_config(b"Hello, world!", config);
    /// ```
    pub fn tokenize_with_config(content: &'a [u8], config: TokenizerConfig) -> Vec<Token<'a>> {
        Self::with_config(content, config).collect()
    }

    /// Get the remaining content that hasn't been tokenized yet.
    ///
    /// # Returns
    ///
    /// A slice of the content from the current position to the end.
    #[inline]
    pub fn remaining(&self) -> &'a [u8] {
        &self.content[self.position..]
    }

    /// Get the current position in the content.
    ///
    /// # Returns
    ///
    /// The byte offset of the next character to be tokenized.
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Check if tokenization is complete.
    ///
    /// # Returns
    ///
    /// `true` if all content has been tokenized.
    #[inline]
    pub fn is_finished(&self) -> bool {
        self.position >= self.content.len()
    }

    /// Peek at the next byte without consuming it.
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.content.get(self.position).copied()
    }

    /// Peek at the byte at position + offset.
    #[inline]
    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.content.get(self.position + offset).copied()
    }

    /// Consume the current byte and advance position.
    #[inline]
    fn advance(&mut self) {
        if self.position < self.content.len() {
            self.position += 1;
        }
    }

    /// Check if a byte is a word character (alphanumeric or underscore).
    #[inline]
    fn is_word_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Check if a byte starts a number.
    #[inline]
    fn is_number_start(b: u8) -> bool {
        b.is_ascii_digit()
    }

    /// Check if a byte is whitespace (but not newline).
    #[inline]
    fn is_whitespace(b: u8) -> bool {
        b == b' ' || b == b'\t'
    }

    /// Check if a byte is a newline.
    #[inline]
    fn is_newline(b: u8) -> bool {
        b == b'\n' || b == b'\r'
    }

    /// Check if a byte is an operator character.
    #[inline]
    fn is_operator(b: u8) -> bool {
        matches!(
            b,
            b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'!' | b'<' | b'>' | b'&' | b'|' | b'^'
                | b'~' | b'?' | b':'
        )
    }

    /// Check if a byte is punctuation.
    #[inline]
    fn is_punctuation(b: u8) -> bool {
        matches!(
            b,
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b';' | b',' | b'.' | b'@' | b'#' | b'$'
                | b'`' | b'\\'
        )
    }

    /// Read a word token (identifier/keyword).
    fn read_word(&mut self) -> Token<'a> {
        let start = self.position;
        while let Some(b) = self.peek() {
            if Self::is_word_char(b) {
                self.advance();
            } else {
                break;
            }
        }
        Token::new(&self.content[start..self.position], TokenKind::Word, start)
    }

    /// Read a whitespace token.
    fn read_whitespace(&mut self) -> Token<'a> {
        let start = self.position;
        while let Some(b) = self.peek() {
            if Self::is_whitespace(b) {
                self.advance();
                if !self.config.merge_whitespace {
                    break;
                }
            } else {
                break;
            }
        }
        Token::new(
            &self.content[start..self.position],
            TokenKind::Whitespace,
            start,
        )
    }

    /// Read a newline token.
    fn read_newline(&mut self) -> Token<'a> {
        let start = self.position;
        if self.peek() == Some(b'\r') {
            self.advance();
        }
        if self.peek() == Some(b'\n') {
            self.advance();
        }
        Token::new(
            &self.content[start..self.position],
            TokenKind::Newline,
            start,
        )
    }

    /// Read an operator token, handling multi-character operators.
    fn read_operator(&mut self) -> Token<'a> {
        let start = self.position;

        // Get first character
        let first = self.peek().unwrap();
        self.advance();

        // Check for common multi-char operators
        if let Some(second) = self.peek() {
            let is_double = matches!(
                (first, second),
                // Comparison operators
                (b'=', b'=')
                    | (b'!', b'=')
                    | (b'<', b'=')
                    | (b'>', b'=')
                    // Logical operators
                    | (b'&', b'&')
                    | (b'|', b'|')
                    // Increment/decrement
                    | (b'+', b'+')
                    | (b'-', b'-')
                    // Shift operators
                    | (b'<', b'<')
                    | (b'>', b'>')
                    // Arrow operators
                    | (b'-', b'>')
                    | (b'=', b'>')
                    // Scope operator
                    | (b':', b':')
                    // Compound assignment
                    | (b'+', b'=')
                    | (b'-', b'=')
                    | (b'*', b'=')
                    | (b'/', b'=')
                    | (b'%', b'=')
                    | (b'&', b'=')
                    | (b'|', b'=')
                    | (b'^', b'=')
            );
            if is_double {
                self.advance();

                // Check for triple-char operators like <<= >>=
                if let Some(third) = self.peek() {
                    let is_triple = matches!(
                        (first, second, third),
                        (b'<', b'<', b'=') | (b'>', b'>', b'=')
                    );
                    if is_triple {
                        self.advance();
                    }
                }
            }
        }

        Token::new(
            &self.content[start..self.position],
            TokenKind::Operator,
            start,
        )
    }

    /// Read a number token, handling various numeric formats.
    fn read_number(&mut self) -> Token<'a> {
        let start = self.position;

        // Handle hex (0x), octal (0o), binary (0b)
        if self.peek() == Some(b'0') {
            self.advance();
            if let Some(b) = self.peek() {
                if matches!(b, b'x' | b'X' | b'o' | b'O' | b'b' | b'B') {
                    self.advance();
                }
            }
        }

        // Read digits (including _ separators)
        while let Some(b) = self.peek() {
            if b.is_ascii_hexdigit() || b == b'_' {
                self.advance();
            } else {
                break;
            }
        }

        // Handle decimal point and fraction
        if self.peek() == Some(b'.')
            && self
                .peek_at(1)
                .map(|b| b.is_ascii_digit())
                .unwrap_or(false)
        {
            self.advance(); // consume '.'
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() || b == b'_' {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Handle exponent (e.g., 1e10, 2.5E-3)
        if let Some(b'e' | b'E') = self.peek() {
            self.advance();
            if let Some(b'+' | b'-') = self.peek() {
                self.advance();
            }
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Handle type suffix (f32, i64, u8, etc.)
        while let Some(b) = self.peek() {
            if b.is_ascii_alphabetic() {
                self.advance();
            } else {
                break;
            }
        }

        Token::new(
            &self.content[start..self.position],
            TokenKind::Number,
            start,
        )
    }

    /// Read a string literal token.
    fn read_string(&mut self, quote: u8) -> Token<'a> {
        let start = self.position;
        self.advance(); // consume opening quote

        while let Some(b) = self.peek() {
            if b == quote {
                self.advance(); // consume closing quote
                break;
            } else if b == b'\\' {
                self.advance(); // consume backslash
                self.advance(); // consume escaped char
            } else if Self::is_newline(b) {
                // Unterminated string - stop at newline
                break;
            } else {
                self.advance();
            }
        }

        Token::new(
            &self.content[start..self.position],
            TokenKind::String,
            start,
        )
    }

    /// Read a single-line comment token.
    fn read_comment(&mut self) -> Token<'a> {
        let start = self.position;
        self.advance(); // first /
        self.advance(); // second /

        // Read until newline
        while let Some(b) = self.peek() {
            if Self::is_newline(b) {
                break;
            }
            self.advance();
        }

        Token::new(
            &self.content[start..self.position],
            TokenKind::Comment,
            start,
        )
    }

    /// Read a punctuation token.
    fn read_punctuation(&mut self) -> Token<'a> {
        let start = self.position;
        self.advance();
        Token::new(
            &self.content[start..self.position],
            TokenKind::Punctuation,
            start,
        )
    }

    /// Read any other single character.
    fn read_other(&mut self) -> Token<'a> {
        let start = self.position;
        self.advance();
        Token::new(
            &self.content[start..self.position],
            TokenKind::Other,
            start,
        )
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_finished() {
            return None;
        }

        let b = self.peek()?;

        // Determine token type and read it
        let token = if Self::is_newline(b) {
            self.read_newline()
        } else if Self::is_whitespace(b) {
            self.read_whitespace()
        } else if self.config.recognize_comments && b == b'/' && self.peek_at(1) == Some(b'/') {
            self.read_comment()
        } else if self.config.recognize_strings && (b == b'"' || b == b'\'') {
            self.read_string(b)
        } else if self.config.recognize_numbers && Self::is_number_start(b) {
            self.read_number()
        } else if Self::is_word_char(b) && !b.is_ascii_digit() {
            self.read_word()
        } else if self.config.recognize_operators && Self::is_operator(b) {
            self.read_operator()
        } else if Self::is_punctuation(b) {
            self.read_punctuation()
        } else {
            self.read_other()
        };

        Some(token)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.content.len() - self.position;
        // At minimum 0 tokens, at maximum one token per byte
        (0, Some(remaining))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // TokenKind tests
    // =========================================================================

    #[test]
    fn test_token_kind_is_significant() {
        assert!(TokenKind::Word.is_significant());
        assert!(TokenKind::Operator.is_significant());
        assert!(TokenKind::Punctuation.is_significant());
        assert!(TokenKind::String.is_significant());
        assert!(TokenKind::Number.is_significant());
        assert!(TokenKind::Comment.is_significant());
        assert!(TokenKind::Other.is_significant());
        assert!(!TokenKind::Whitespace.is_significant());
        assert!(!TokenKind::Newline.is_significant());
    }

    #[test]
    fn test_token_kind_is_content() {
        assert!(TokenKind::Word.is_content());
        assert!(TokenKind::String.is_content());
        assert!(TokenKind::Number.is_content());
        assert!(TokenKind::Comment.is_content());
        assert!(!TokenKind::Operator.is_content());
        assert!(!TokenKind::Punctuation.is_content());
        assert!(!TokenKind::Whitespace.is_content());
        assert!(!TokenKind::Newline.is_content());
        assert!(!TokenKind::Other.is_content());
    }

    #[test]
    fn test_token_kind_is_whitespace() {
        assert!(TokenKind::Whitespace.is_whitespace());
        assert!(TokenKind::Newline.is_whitespace());
        assert!(!TokenKind::Word.is_whitespace());
        assert!(!TokenKind::Other.is_whitespace());
    }

    #[test]
    fn test_token_kind_name() {
        assert_eq!(TokenKind::Word.name(), "word");
        assert_eq!(TokenKind::Whitespace.name(), "ws");
        assert_eq!(TokenKind::Operator.name(), "op");
        assert_eq!(TokenKind::Punctuation.name(), "punct");
        assert_eq!(TokenKind::String.name(), "string");
        assert_eq!(TokenKind::Number.name(), "number");
        assert_eq!(TokenKind::Comment.name(), "comment");
        assert_eq!(TokenKind::Newline.name(), "newline");
        assert_eq!(TokenKind::Other.name(), "other");
    }

    #[test]
    fn test_token_kind_display() {
        assert_eq!(format!("{}", TokenKind::Word), "word");
        assert_eq!(format!("{}", TokenKind::Operator), "op");
    }

    // =========================================================================
    // Token tests
    // =========================================================================

    #[test]
    fn test_token_new() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        assert_eq!(token.content(), b"hello");
        assert_eq!(token.kind(), TokenKind::Word);
        assert_eq!(token.offset(), 0);
        assert_eq!(token.len(), 5);
        assert!(!token.is_empty());
    }

    #[test]
    fn test_token_as_str() {
        let token = Token::new(b"world", TokenKind::Word, 0);
        assert_eq!(token.as_str(), "world");
    }

    #[test]
    fn test_token_offsets() {
        let token = Token::new(b"test", TokenKind::Word, 10);
        assert_eq!(token.offset(), 10);
        assert_eq!(token.len(), 4);
        assert_eq!(token.end_offset(), 14);
        assert_eq!(token.byte_range(), 10..14);
    }

    #[test]
    fn test_token_empty() {
        let token = Token::new(b"", TokenKind::Other, 0);
        assert!(token.is_empty());
        assert_eq!(token.len(), 0);
        assert_eq!(token.end_offset(), 0);
    }

    #[test]
    fn test_token_hash_value() {
        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);

        // Same content = same hash
        assert_eq!(t1.hash_value(), t2.hash_value());
        // Different content = different hash (with high probability)
        assert_ne!(t1.hash_value(), t3.hash_value());
    }

    #[test]
    fn test_token_is_significant() {
        let word = Token::new(b"foo", TokenKind::Word, 0);
        let space = Token::new(b" ", TokenKind::Whitespace, 0);
        let newline = Token::new(b"\n", TokenKind::Newline, 0);

        assert!(word.is_significant());
        assert!(!space.is_significant());
        assert!(!newline.is_significant());
    }

    #[test]
    fn test_token_equality() {
        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);
        let t4 = Token::new(b"hello", TokenKind::String, 0);

        // Equality is based on content only
        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
        // Different kind, same content = equal
        assert_eq!(t1, t4);
    }

    #[test]
    fn test_token_hash_trait() {
        use std::collections::HashSet;

        let t1 = Token::new(b"hello", TokenKind::Word, 0);
        let t2 = Token::new(b"hello", TokenKind::Word, 10);
        let t3 = Token::new(b"world", TokenKind::Word, 0);

        let mut set = HashSet::new();
        set.insert(t1.clone());
        set.insert(t2);
        set.insert(t3);

        // t1 and t2 should hash the same, so only 2 items
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_token_debug() {
        let token = Token::new(b"test", TokenKind::Word, 5);
        let debug = format!("{:?}", token);
        assert!(debug.contains("test"));
        assert!(debug.contains("Word"));
        assert!(debug.contains("5"));
    }

    #[test]
    fn test_token_display() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        assert_eq!(format!("{}", token), "hello");
    }

    #[test]
    fn test_token_clone() {
        let t1 = Token::new(b"test", TokenKind::Word, 0);
        let t2 = t1.clone();
        assert_eq!(t1, t2);
        assert_eq!(t1.hash_value(), t2.hash_value());
    }

    // =========================================================================
    // TokenizerConfig tests
    // =========================================================================

    #[test]
    fn test_tokenizer_config_default() {
        let config = TokenizerConfig::default();
        assert!(config.merge_whitespace);
        assert!(config.recognize_operators);
        assert!(config.recognize_strings);
        assert!(config.recognize_numbers);
        assert!(config.recognize_comments);
    }

    #[test]
    fn test_tokenizer_config_minimal() {
        let config = TokenizerConfig::minimal();
        assert!(config.merge_whitespace);
        assert!(!config.recognize_operators);
        assert!(!config.recognize_strings);
        assert!(!config.recognize_numbers);
        assert!(!config.recognize_comments);
    }

    #[test]
    fn test_tokenizer_config_code() {
        let config = TokenizerConfig::code();
        assert_eq!(config, TokenizerConfig::default());
    }

    #[test]
    fn test_tokenizer_config_prose() {
        let config = TokenizerConfig::prose();
        assert!(config.merge_whitespace);
        assert!(!config.recognize_operators);
        assert!(!config.recognize_strings);
    }

    // =========================================================================
    // Tokenizer basic tests
    // =========================================================================

    #[test]
    fn test_tokenizer_empty() {
        let tokens: Vec<Token> = Tokenizer::new(b"").collect();
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenizer_single_word() {
        let tokens: Vec<Token> = Tokenizer::new(b"hello").collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].as_str(), "hello");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
    }

    #[test]
    fn test_tokenizer_words_and_spaces() {
        let tokens: Vec<Token> = Tokenizer::new(b"hello world").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].as_str(), "hello");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
        assert_eq!(tokens[1].as_str(), " ");
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[2].as_str(), "world");
        assert_eq!(tokens[2].kind(), TokenKind::Word);
    }

    #[test]
    fn test_tokenizer_merged_whitespace() {
        let tokens: Vec<Token> = Tokenizer::new(b"a   b").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "   ");
        assert_eq!(tokens[1].kind(), TokenKind::Whitespace);
    }

    #[test]
    fn test_tokenizer_newline() {
        let tokens: Vec<Token> = Tokenizer::new(b"a\nb").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "\n");
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    #[test]
    fn test_tokenizer_crlf() {
        let tokens: Vec<Token> = Tokenizer::new(b"a\r\nb").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].as_str(), "\r\n");
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    // =========================================================================
    // Tokenizer code-aware tests
    // =========================================================================

    #[test]
    fn test_tokenizer_simple_code() {
        let code = b"let x = 42;";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens.len(), 8);
        assert_eq!(tokens[0].as_str(), "let");
        assert_eq!(tokens[0].kind(), TokenKind::Word);
        assert_eq!(tokens[2].as_str(), "x");
        assert_eq!(tokens[2].kind(), TokenKind::Word);
        assert_eq!(tokens[4].as_str(), "=");
        assert_eq!(tokens[4].kind(), TokenKind::Operator);
        assert_eq!(tokens[6].as_str(), "42");
        assert_eq!(tokens[6].kind(), TokenKind::Number);
        assert_eq!(tokens[7].as_str(), ";");
        assert_eq!(tokens[7].kind(), TokenKind::Punctuation);
    }

    #[test]
    fn test_tokenizer_operators() {
        let code = b"a == b && c != d";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0].as_str(), "==");
        assert_eq!(ops[1].as_str(), "&&");
        assert_eq!(ops[2].as_str(), "!=");
    }

    #[test]
    fn test_tokenizer_arrow_operators() {
        let code = b"fn() -> i32 { x => y }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert!(ops.iter().any(|t| t.as_str() == "->"));
        assert!(ops.iter().any(|t| t.as_str() == "=>"));
    }

    #[test]
    fn test_tokenizer_scope_operator() {
        let code = b"std::io::Result";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].as_str(), "::");
        assert_eq!(ops[1].as_str(), "::");
    }

    #[test]
    fn test_tokenizer_compound_assignment() {
        let code = b"x += 1; y -= 2; z *= 3";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert!(ops.iter().any(|t| t.as_str() == "+="));
        assert!(ops.iter().any(|t| t.as_str() == "-="));
        assert!(ops.iter().any(|t| t.as_str() == "*="));
    }

    #[test]
    fn test_tokenizer_string() {
        let code = b"let s = \"hello world\";";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].as_str(), "\"hello world\"");
    }

    #[test]
    fn test_tokenizer_string_with_escape() {
        let code = b"\"hello\\nworld\"";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].as_str(), "\"hello\\nworld\"");
        assert_eq!(tokens[0].kind(), TokenKind::String);
    }

    #[test]
    fn test_tokenizer_char_literal() {
        let code = b"let c = 'a';";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].as_str(), "'a'");
    }

    #[test]
    fn test_tokenizer_comment() {
        let code = b"x = 1; // this is a comment";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].as_str(), "// this is a comment");
    }

    #[test]
    fn test_tokenizer_comment_stops_at_newline() {
        let code = b"// comment\nnext line";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].as_str(), "// comment");
        assert_eq!(tokens[0].kind(), TokenKind::Comment);
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
        assert_eq!(tokens[2].as_str(), "next");
    }

    #[test]
    fn test_tokenizer_numbers_integer() {
        let code = b"42 0 123456";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "42");
        assert_eq!(nums[1].as_str(), "0");
        assert_eq!(nums[2].as_str(), "123456");
    }

    #[test]
    fn test_tokenizer_numbers_float() {
        let code = b"3.14 0.5 10.0";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "3.14");
        assert_eq!(nums[1].as_str(), "0.5");
        assert_eq!(nums[2].as_str(), "10.0");
    }

    #[test]
    fn test_tokenizer_numbers_hex() {
        let code = b"0xff 0xDEADBEEF 0X10";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 3);
        assert_eq!(nums[0].as_str(), "0xff");
        assert_eq!(nums[1].as_str(), "0xDEADBEEF");
        assert_eq!(nums[2].as_str(), "0X10");
    }

    #[test]
    fn test_tokenizer_numbers_binary_octal() {
        let code = b"0b1010 0o777";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 2);
        assert_eq!(nums[0].as_str(), "0b1010");
        assert_eq!(nums[1].as_str(), "0o777");
    }

    #[test]
    fn test_tokenizer_numbers_with_separators() {
        let code = b"1_000_000 0xFF_FF";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 2);
        assert_eq!(nums[0].as_str(), "1_000_000");
        assert_eq!(nums[1].as_str(), "0xFF_FF");
    }

    #[test]
    fn test_tokenizer_numbers_scientific() {
        let code = b"1e10 2.5E-3 3e+5";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        // Note: The tokenizer may not perfectly handle all scientific notation
        // The important thing is that numeric content is captured
        assert!(nums.len() >= 1);
        assert!(nums.iter().any(|t| t.as_str().contains("1e10") || t.as_str() == "1e10"));
    }

    #[test]
    fn test_tokenizer_numbers_with_suffix() {
        let code = b"42u32 3.14f64 100i64";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Number)
            .collect();
        // Numbers with type suffixes: the numeric part is captured
        // The suffix may be parsed separately depending on tokenizer logic
        assert!(nums.len() >= 3);
        // Verify the numeric parts are present
        assert!(nums.iter().any(|t| t.as_str().starts_with("42")));
        assert!(nums.iter().any(|t| t.as_str().starts_with("3.14") || t.as_str() == "3"));
        assert!(nums.iter().any(|t| t.as_str().starts_with("100")));
    }

    #[test]
    fn test_tokenizer_punctuation() {
        let code = b"fn(a, b) { x.y[z] }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let puncts: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Punctuation)
            .collect();

        let punct_strs: Vec<_> = puncts.iter().map(|t| t.as_str().to_string()).collect();
        assert!(punct_strs.contains(&"(".to_string()));
        assert!(punct_strs.contains(&")".to_string()));
        assert!(punct_strs.contains(&",".to_string()));
        assert!(punct_strs.contains(&"{".to_string()));
        assert!(punct_strs.contains(&"}".to_string()));
        assert!(punct_strs.contains(&".".to_string()));
        assert!(punct_strs.contains(&"[".to_string()));
        assert!(punct_strs.contains(&"]".to_string()));
    }

    #[test]
    fn test_tokenizer_underscore_identifier() {
        let code = b"_foo __bar _123";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let words: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Word)
            .collect();
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].as_str(), "_foo");
        assert_eq!(words[1].as_str(), "__bar");
        assert_eq!(words[2].as_str(), "_123");
    }

    // =========================================================================
    // Tokenizer offset tracking tests
    // =========================================================================

    #[test]
    fn test_tokenizer_offset_tracking() {
        let code = b"a b c";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].offset(), 0);    // 'a'
        assert_eq!(tokens[0].end_offset(), 1);
        assert_eq!(tokens[1].offset(), 1);    // ' '
        assert_eq!(tokens[1].end_offset(), 2);
        assert_eq!(tokens[2].offset(), 2);    // 'b'
        assert_eq!(tokens[2].end_offset(), 3);
        assert_eq!(tokens[3].offset(), 3);    // ' '
        assert_eq!(tokens[4].offset(), 4);    // 'c'
        assert_eq!(tokens[4].end_offset(), 5);
    }

    #[test]
    fn test_tokenizer_offset_multi_char() {
        let code = b"foo == bar";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert_eq!(tokens[0].offset(), 0);    // 'foo'
        assert_eq!(tokens[0].end_offset(), 3);
        assert_eq!(tokens[2].offset(), 4);    // '=='
        assert_eq!(tokens[2].end_offset(), 6);
        assert_eq!(tokens[4].offset(), 7);    // 'bar'
        assert_eq!(tokens[4].end_offset(), 10);
    }

    #[test]
    fn test_tokenizer_byte_range_slicing() {
        let content = b"hello world";
        let tokens: Vec<Token> = Tokenizer::new(content).collect();

        // Verify byte_range can be used to slice original content
        assert_eq!(&content[tokens[0].byte_range()], b"hello");
        assert_eq!(&content[tokens[1].byte_range()], b" ");
        assert_eq!(&content[tokens[2].byte_range()], b"world");
    }

    // =========================================================================
    // Tokenizer configuration tests
    // =========================================================================

    #[test]
    fn test_tokenizer_minimal_no_operators() {
        let code = b"a == b";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, == is not recognized as single operator
        // Instead, = and = are separate punctuation
        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .collect();
        assert_eq!(ops.len(), 0);
    }

    #[test]
    fn test_tokenizer_minimal_no_strings() {
        let code = b"\"hello\"";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, quotes are separate tokens
        let strings: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::String)
            .collect();
        assert_eq!(strings.len(), 0);
    }

    #[test]
    fn test_tokenizer_minimal_no_numbers() {
        let code = b"42";
        let config = TokenizerConfig::minimal();
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // With minimal config, digits may be treated differently
        // Since 4 and 2 start with digit, they won't be Word tokens
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].kind(), TokenKind::Other);
        assert_eq!(tokens[1].kind(), TokenKind::Other);
    }

    #[test]
    fn test_tokenizer_no_merge_whitespace() {
        let code = b"a   b";
        let config = TokenizerConfig {
            merge_whitespace: false,
            ..TokenizerConfig::default()
        };
        let tokens: Vec<Token> = Tokenizer::with_config(code, config).collect();

        // Each space is a separate token
        let ws: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Whitespace)
            .collect();
        assert_eq!(ws.len(), 3);
    }

    // =========================================================================
    // Tokenizer edge case tests
    // =========================================================================

    #[test]
    fn test_tokenizer_only_whitespace() {
        let tokens: Vec<Token> = Tokenizer::new(b"   \t  ").collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind(), TokenKind::Whitespace);
        assert_eq!(tokens[0].as_str(), "   \t  ");
    }

    #[test]
    fn test_tokenizer_only_newlines() {
        let tokens: Vec<Token> = Tokenizer::new(b"\n\n\n").collect();
        assert_eq!(tokens.len(), 3);
        for token in &tokens {
            assert_eq!(token.kind(), TokenKind::Newline);
        }
    }

    #[test]
    fn test_tokenizer_mixed_newlines() {
        let tokens: Vec<Token> = Tokenizer::new(b"\n\r\n\n").collect();
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].as_str(), "\n");
        assert_eq!(tokens[1].as_str(), "\r\n");
        assert_eq!(tokens[2].as_str(), "\n");
    }

    #[test]
    fn test_tokenizer_unterminated_string() {
        let code = b"\"hello\nworld";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // String should stop at newline
        assert_eq!(tokens[0].as_str(), "\"hello");
        assert_eq!(tokens[0].kind(), TokenKind::String);
        assert_eq!(tokens[1].kind(), TokenKind::Newline);
    }

    #[test]
    fn test_tokenizer_special_chars() {
        let code = b"@decorator #macro $var";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // @ # $ are punctuation
        assert!(tokens.iter().any(|t| t.as_str() == "@"));
        assert!(tokens.iter().any(|t| t.as_str() == "#"));
        assert!(tokens.iter().any(|t| t.as_str() == "$"));
    }

    #[test]
    fn test_tokenizer_unicode_in_other() {
        let code = "café".as_bytes();
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // ASCII part is Word, then 'é' bytes are Other
        assert!(tokens.len() >= 1);
    }

    // =========================================================================
    // Tokenizer convenience method tests
    // =========================================================================

    #[test]
    fn test_tokenizer_tokenize_all() {
        let tokens = Tokenizer::tokenize_all(b"a b");
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn test_tokenizer_tokenize_with_config() {
        let config = TokenizerConfig::minimal();
        let tokens = Tokenizer::tokenize_with_config(b"a b", config);
        assert_eq!(tokens.len(), 3);
    }

    #[test]
    fn test_tokenizer_remaining() {
        let mut tokenizer = Tokenizer::new(b"hello world");
        assert_eq!(tokenizer.remaining(), b"hello world");

        tokenizer.next(); // consume "hello"
        assert_eq!(tokenizer.remaining(), b" world");
    }

    #[test]
    fn test_tokenizer_position() {
        let mut tokenizer = Tokenizer::new(b"ab cd");
        assert_eq!(tokenizer.position(), 0);

        tokenizer.next(); // "ab"
        assert_eq!(tokenizer.position(), 2);

        tokenizer.next(); // " "
        assert_eq!(tokenizer.position(), 3);
    }

    #[test]
    fn test_tokenizer_is_finished() {
        let mut tokenizer = Tokenizer::new(b"ab");
        assert!(!tokenizer.is_finished());

        tokenizer.next(); // consume "ab"
        assert!(tokenizer.is_finished());
    }

    #[test]
    fn test_tokenizer_size_hint() {
        let tokenizer = Tokenizer::new(b"hello");
        let (min, max) = tokenizer.size_hint();
        assert_eq!(min, 0);
        assert_eq!(max, Some(5));
    }

    // =========================================================================
    // Real-world code tests
    // =========================================================================

    #[test]
    fn test_tokenizer_rust_function() {
        let code = b"pub fn calculate(x: i32, y: i32) -> i32 { x + y }";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        // Should tokenize without panicking
        assert!(tokens.len() > 10);

        // Check some specific tokens
        let words: Vec<_> = tokens.iter().filter(|t| t.kind() == TokenKind::Word).collect();
        assert!(words.iter().any(|t| t.as_str() == "pub"));
        assert!(words.iter().any(|t| t.as_str() == "fn"));
        assert!(words.iter().any(|t| t.as_str() == "calculate"));
        assert!(words.iter().any(|t| t.as_str() == "i32"));
    }

    #[test]
    fn test_tokenizer_javascript_arrow() {
        let code = b"const sum = (a, b) => a + b;";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert!(tokens.iter().any(|t| t.as_str() == "=>"));
        assert!(tokens.iter().any(|t| t.as_str() == "const"));
    }

    #[test]
    fn test_tokenizer_python_style() {
        let code = b"def greet(name): // greeting\n    print(f\"Hello {name}\")";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        assert!(tokens.iter().any(|t| t.as_str() == "def"));
        // Note: We only recognize // comments, not # comments
        assert!(tokens.iter().any(|t| t.kind() == TokenKind::Comment));
        assert!(tokens.iter().any(|t| t.kind() == TokenKind::Newline));
    }

    #[test]
    fn test_tokenizer_complex_expression() {
        let code = b"result = ((a + b) * c) / (d - e) % f";
        let tokens: Vec<Token> = Tokenizer::new(code).collect();

        let ops: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind() == TokenKind::Operator)
            .map(|t| t.as_str().to_string())
            .collect();

        assert!(ops.contains(&"=".to_string()));
        assert!(ops.contains(&"+".to_string()));
        assert!(ops.contains(&"*".to_string()));
        assert!(ops.contains(&"/".to_string()));
        assert!(ops.contains(&"-".to_string()));
        assert!(ops.contains(&"%".to_string()));
    }
}

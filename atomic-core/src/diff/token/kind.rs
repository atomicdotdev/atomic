use super::*;

/// The kind of token, used for semantic categorization.
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

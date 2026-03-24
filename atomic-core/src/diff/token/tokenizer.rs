use super::*;

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
            b'+' | b'-'
                | b'*'
                | b'/'
                | b'%'
                | b'='
                | b'!'
                | b'<'
                | b'>'
                | b'&'
                | b'|'
                | b'^'
                | b'~'
                | b'?'
                | b':'
        )
    }

    /// Check if a byte is punctuation.
    #[inline]
    fn is_punctuation(b: u8) -> bool {
        matches!(
            b,
            b'(' | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b';'
                | b','
                | b'.'
                | b'@'
                | b'#'
                | b'$'
                | b'`'
                | b'\\'
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
        if self.peek() == Some(b'.') && self.peek_at(1).map(|b| b.is_ascii_digit()).unwrap_or(false)
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
        Token::new(&self.content[start..self.position], TokenKind::Other, start)
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

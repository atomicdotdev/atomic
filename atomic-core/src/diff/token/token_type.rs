use super::*;


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

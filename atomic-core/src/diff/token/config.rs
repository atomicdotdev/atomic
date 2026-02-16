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

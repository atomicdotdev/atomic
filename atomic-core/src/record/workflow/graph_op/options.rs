//! Hunk build options configuration.
//!
//! Contains [`HunkBuildOptions`] which controls how diff operations are
//! converted into hunks during the recording pipeline.

use crate::change::Encoding;

// HUNK BUILD OPTIONS

/// Options for building hunks from diff operations.
///
/// Controls how hunks are constructed, including encoding information
/// and context line settings.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
/// use atomic_core::change::Encoding;
///
/// let options = HunkBuildOptions::new()
///     .encoding(Encoding::Utf8)
///     .context_lines(3)
///     .include_function_context(true);
///
/// assert_eq!(options.get_encoding(), Some(Encoding::Utf8));
/// assert_eq!(options.get_context_lines(), 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkBuildOptions {
    /// Text encoding for the file being processed.
    ///
    /// `None` indicates binary content.
    pub(crate) encoding: Option<Encoding>,

    /// Number of unchanged lines to include as context.
    ///
    /// Context lines help users understand where changes occur
    /// and are used for display purposes.
    pub(crate) context_lines: usize,

    /// Whether to include function/class context in output.
    ///
    /// When enabled, hunks include information about the enclosing
    /// function or class for better readability.
    pub(crate) include_function_context: bool,

    /// Minimum number of unchanged lines between hunks.
    ///
    /// If fewer than this many unchanged lines separate two changes,
    /// they are combined into a single graph_op.
    pub(crate) combine_threshold: usize,
}

impl HunkBuildOptions {
    /// Default number of context lines to include.
    pub const DEFAULT_CONTEXT_LINES: usize = 3;

    /// Default threshold for combining adjacent hunks.
    pub const DEFAULT_COMBINE_THRESHOLD: usize = 6;

    /// Create new options with default values.
    ///
    /// Default values:
    /// - `encoding`: None (binary)
    /// - `context_lines`: 3
    /// - `include_function_context`: false
    /// - `combine_threshold`: 6
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new();
    /// assert_eq!(options.get_context_lines(), 3);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the text encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The encoding to use (UTF-8, Binary, etc.)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    /// assert_eq!(options.get_encoding(), Some(Encoding::Utf8));
    /// ```
    #[must_use]
    pub fn encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = Some(encoding);
        self
    }

    /// Set the encoding to None (binary content).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().binary();
    /// assert!(options.get_encoding().is_none());
    /// ```
    #[must_use]
    pub fn binary(mut self) -> Self {
        self.encoding = None;
        self
    }

    /// Set the number of context lines.
    ///
    /// # Arguments
    ///
    /// * `lines` - Number of unchanged lines to include around changes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().context_lines(5);
    /// assert_eq!(options.get_context_lines(), 5);
    /// ```
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Enable or disable function context inclusion.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include enclosing function/class names
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().include_function_context(true);
    /// assert!(options.get_include_function_context());
    /// ```
    #[must_use]
    pub fn include_function_context(mut self, include: bool) -> Self {
        self.include_function_context = include;
        self
    }

    /// Set the combine threshold.
    ///
    /// Hunks separated by fewer than this many unchanged lines
    /// will be merged into a single graph_op.
    ///
    /// # Arguments
    ///
    /// * `threshold` - Minimum unchanged lines between separate hunks
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().combine_threshold(10);
    /// assert_eq!(options.get_combine_threshold(), 10);
    /// ```
    #[must_use]
    pub fn combine_threshold(mut self, threshold: usize) -> Self {
        self.combine_threshold = threshold;
        self
    }

    /// Get the encoding setting.
    #[must_use]
    pub fn get_encoding(&self) -> Option<Encoding> {
        self.encoding
    }

    /// Get the context lines setting.
    #[must_use]
    pub fn get_context_lines(&self) -> usize {
        self.context_lines
    }

    /// Get the function context inclusion setting.
    #[must_use]
    pub fn get_include_function_context(&self) -> bool {
        self.include_function_context
    }

    /// Get the combine threshold setting.
    #[must_use]
    pub fn get_combine_threshold(&self) -> usize {
        self.combine_threshold
    }

    /// Check if content should be treated as binary.
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.encoding.is_none()
    }
}

impl Default for HunkBuildOptions {
    fn default() -> Self {
        Self {
            encoding: None,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
            include_function_context: false,
            combine_threshold: Self::DEFAULT_COMBINE_THRESHOLD,
        }
    }
}

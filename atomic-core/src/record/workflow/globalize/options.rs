use super::*;


// CONFIGURATION

/// Configuration options for globalization.
///
/// Controls how local hunks are converted to graph operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::GlobalizeOptions;
///
/// let options = GlobalizeOptions::new()
///     .with_include_empty_files(false)
///     .with_validate_positions(true);
///
/// assert!(!options.include_empty_files());
/// assert!(options.validate_positions());
/// ```
#[derive(Debug, Clone)]
pub struct GlobalizeOptions {
    /// Whether to include empty files in the output.
    ///
    /// If false, files with no content hunks are skipped.
    /// Default: false
    include_empty_files: bool,

    /// Whether to validate that positions exist in the graph.
    ///
    /// Enabling this adds overhead but catches errors early.
    /// Default: true
    validate_positions: bool,

    /// Maximum content size per graph_op (bytes).
    ///
    /// Larger hunks are split. 0 means no limit.
    /// Default: 0 (no limit)
    max_hunk_size: usize,

    /// Default encoding for files without detected encoding.
    ///
    /// Default: UTF-8
    default_encoding: Encoding,
}

impl GlobalizeOptions {
    /// Create new options with default values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new();
    /// assert!(options.validate_positions());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include empty files.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include files with no content
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().with_include_empty_files(true);
    /// assert!(options.include_empty_files());
    /// ```
    #[must_use]
    pub fn with_include_empty_files(mut self, include: bool) -> Self {
        self.include_empty_files = include;
        self
    }

    /// Set whether to validate positions.
    ///
    /// # Arguments
    ///
    /// * `validate` - Whether to validate graph positions
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().with_validate_positions(false);
    /// assert!(!options.validate_positions());
    /// ```
    #[must_use]
    pub fn with_validate_positions(mut self, validate: bool) -> Self {
        self.validate_positions = validate;
        self
    }

    /// Set maximum graph_op size.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum bytes per graph_op (0 = no limit)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().max_hunk_size(1024 * 1024);
    /// assert_eq!(options.max_hunk_size(), 1024 * 1024);
    /// ```
    #[must_use]
    pub fn with_max_hunk_size(mut self, size: usize) -> Self {
        self.max_hunk_size = size;
        self
    }

    /// Set default encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - Default encoding for files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = GlobalizeOptions::new().default_encoding(Encoding::Binary);
    /// assert_eq!(options.default_encoding(), Encoding::Binary);
    /// ```
    #[must_use]
    pub fn with_default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = encoding;
        self
    }

    /// Get whether empty files are included.
    #[must_use]
    pub fn include_empty_files(&self) -> bool {
        self.include_empty_files
    }

    /// Get whether positions are validated.
    #[must_use]
    pub fn validate_positions(&self) -> bool {
        self.validate_positions
    }

    /// Get maximum graph_op size.
    #[must_use]
    pub fn max_hunk_size(&self) -> usize {
        self.max_hunk_size
    }

    /// Get default encoding.
    #[must_use]
    pub fn default_encoding(&self) -> Encoding {
        self.default_encoding
    }
}

impl Default for GlobalizeOptions {
    fn default() -> Self {
        Self {
            include_empty_files: false,
            validate_positions: true,
            max_hunk_size: 0,
            default_encoding: Encoding::Utf8,
        }
    }
}

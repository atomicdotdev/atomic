//! Recording options configuration.
//!
//! Contains [`RecordingOptions`] which controls how detected changes are
//! converted into recorded hunks during the recording pipeline.

use crate::change::Encoding;
use crate::diff::Algorithm;
use crate::record::workflow::graph_op::HunkBuildOptions;

/// Options controlling the recording process.
///
/// These options allow fine-tuning how detected changes are converted
/// into recorded hunks.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::record::RecordingOptions;
/// use atomic_core::diff::Algorithm;
/// use atomic_core::change::Encoding;
///
/// let options = RecordingOptions::new()
///     .algorithm(Algorithm::Patience)
///     .default_encoding(Encoding::Utf8)
///     .max_file_size(10 * 1024 * 1024)
///     .skip_binary(false);
///
/// assert_eq!(options.get_algorithm(), Algorithm::Patience);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingOptions {
    /// Diff algorithm to use for content comparison.
    algorithm: Algorithm,

    /// Default encoding for new files.
    ///
    /// This is used when encoding cannot be auto-detected.
    default_encoding: Option<Encoding>,

    /// Maximum file size to record (in bytes).
    ///
    /// Files larger than this are skipped or marked as binary.
    max_file_size: Option<usize>,

    /// Whether to skip binary files entirely.
    ///
    /// When true, binary files are not recorded. When false, they
    /// are recorded but without diff information.
    skip_binary: bool,

    /// Whether to record empty files.
    record_empty_files: bool,

    /// Number of context lines for hunks.
    context_lines: usize,

    /// Force a whole-file replace instead of a positional diff against
    /// `old_content`.
    ///
    /// Set this when the caller's `old_content` came from resolving a fork
    /// or cyclic conflict in the graph (see
    /// `retrieve_content_with_filter_and_fork_info` in the record path) —
    /// that resolution isn't guaranteed to structurally match what a plain
    /// checkout renders for the same graph state (POMO-2), so a positional
    /// diff against it can produce a corrupted hunk. A whole-file replace
    /// only needs "what's alive gets deleted, this is inserted fresh" to be
    /// true, which holds regardless of how `old_content` was resolved.
    force_whole_file_replace: bool,
}

impl RecordingOptions {
    /// Default maximum file size (50 MB).
    pub const DEFAULT_MAX_FILE_SIZE: usize = 50 * 1024 * 1024;

    /// Default number of context lines.
    pub const DEFAULT_CONTEXT_LINES: usize = 3;

    /// Create new options with defaults.
    ///
    /// Default values:
    /// - `algorithm`: Myers
    /// - `default_encoding`: None (auto-detect)
    /// - `max_file_size`: 50 MB
    /// - `skip_binary`: false
    /// - `record_empty_files`: true
    /// - `context_lines`: 3
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new();
    /// assert!(!options.get_skip_binary());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - The algorithm to use for diffing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let options = RecordingOptions::new().algorithm(Algorithm::Patience);
    /// assert_eq!(options.get_algorithm(), Algorithm::Patience);
    /// ```
    #[must_use]
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set the default encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The default encoding for new files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = RecordingOptions::new().default_encoding(Encoding::Utf8);
    /// assert_eq!(options.get_default_encoding(), Some(Encoding::Utf8));
    /// ```
    #[must_use]
    pub fn default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = Some(encoding);
        self
    }

    /// Set the maximum file size.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().max_file_size(1024 * 1024);
    /// assert_eq!(options.get_max_file_size(), Some(1024 * 1024));
    /// ```
    #[must_use]
    pub fn max_file_size(mut self, size: usize) -> Self {
        self.max_file_size = Some(size);
        self
    }

    /// Set whether to skip binary files.
    ///
    /// # Arguments
    ///
    /// * `skip` - Whether to skip binary files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().skip_binary(true);
    /// assert!(options.get_skip_binary());
    /// ```
    #[must_use]
    pub fn skip_binary(mut self, skip: bool) -> Self {
        self.skip_binary = skip;
        self
    }

    /// Set whether to record empty files.
    ///
    /// # Arguments
    ///
    /// * `record` - Whether to record empty files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().record_empty_files(false);
    /// assert!(!options.get_record_empty_files());
    /// ```
    #[must_use]
    pub fn record_empty_files(mut self, record: bool) -> Self {
        self.record_empty_files = record;
        self
    }

    /// Set the number of context lines.
    ///
    /// # Arguments
    ///
    /// * `lines` - Number of context lines
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().context_lines(5);
    /// assert_eq!(options.get_context_lines(), 5);
    /// ```
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Force a whole-file replace instead of a positional diff against
    /// `old_content`.
    ///
    /// # Arguments
    ///
    /// * `force` - Whether to force a whole-file replace
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().force_whole_file_replace(true);
    /// assert!(options.get_force_whole_file_replace());
    /// ```
    #[must_use]
    pub fn force_whole_file_replace(mut self, force: bool) -> Self {
        self.force_whole_file_replace = force;
        self
    }

    /// Get the algorithm setting.
    #[must_use]
    pub fn get_algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Get the default encoding setting.
    #[must_use]
    pub fn get_default_encoding(&self) -> Option<Encoding> {
        self.default_encoding
    }

    /// Get the max file size setting.
    #[must_use]
    pub fn get_max_file_size(&self) -> Option<usize> {
        self.max_file_size
    }

    /// Get the skip binary setting.
    #[must_use]
    pub fn get_skip_binary(&self) -> bool {
        self.skip_binary
    }

    /// Get the record empty files setting.
    #[must_use]
    pub fn get_record_empty_files(&self) -> bool {
        self.record_empty_files
    }

    /// Get the context lines setting.
    #[must_use]
    pub fn get_context_lines(&self) -> usize {
        self.context_lines
    }

    /// Get the force-whole-file-replace setting.
    #[must_use]
    pub fn get_force_whole_file_replace(&self) -> bool {
        self.force_whole_file_replace
    }

    /// Check if a file size exceeds the maximum.
    #[must_use]
    pub fn exceeds_max_size(&self, size: usize) -> bool {
        self.max_file_size.is_some_and(|max| size > max)
    }

    /// Convert to graph_op build options.
    #[must_use]
    pub fn to_hunk_options(&self) -> HunkBuildOptions {
        let mut opts = HunkBuildOptions::new().context_lines(self.context_lines);
        if let Some(enc) = self.default_encoding {
            opts = opts.encoding(enc);
        }
        opts
    }
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            default_encoding: None,
            max_file_size: Some(Self::DEFAULT_MAX_FILE_SIZE),
            skip_binary: false,
            record_empty_files: true,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
            force_whole_file_replace: false,
        }
    }
}

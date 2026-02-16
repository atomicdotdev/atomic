//! Workflow options for change detection and recording.
//!
//! This module provides configuration options for controlling the behavior
//! of change detection and recording workflows. Options are designed to be
//! composable via builder pattern methods.
//!
//! # Overview
//!
//! The workflow options control:
//!
//! - **Detection behavior**: Algorithm choice, mtime optimization, move detection
//! - **Filtering**: Path prefixes, file exclusions
//! - **Performance**: Max file sizes, parallelism hints
//! - **Error handling**: How to handle missing files, encoding issues
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::workflow::WorkflowOptions;
//! use atomic_core::diff::Algorithm;
//!
//! // Create options with defaults
//! let opts = WorkflowOptions::new();
//!
//! // Customize via builder pattern
//! let opts = WorkflowOptions::new()
//!     .with_algorithm(Algorithm::Patience)
//!     .with_check_mtime(false)
//!     .with_prefix("src/")
//!     .with_max_file_size(10 * 1024 * 1024);  // 10MB
//! ```

use crate::diff::Algorithm;

// WORKFLOW OPTIONS

/// Configuration options for change detection and recording workflows.
///
/// This structure controls the behavior of `detect_changes()`, `record()`,
/// and related functions. Use the builder methods to customize behavior.
///
/// # Default Values
///
/// | Option | Default | Description |
/// |--------|---------|-------------|
/// | `algorithm` | `Myers` | Diff algorithm for content comparison |
/// | `check_mtime` | `true` | Use mtime to skip unchanged files |
/// | `detect_moves` | `true` | Attempt to detect file moves |
/// | `detect_encoding` | `true` | Track encoding changes |
/// | `detect_permissions` | `true` | Track permission changes |
/// | `prefix` | `""` | Path prefix filter (empty = all) |
/// | `max_file_size` | `10MB` | Max size for text diffing |
/// | `force_rediff` | `false` | Always diff, ignore optimizations |
/// | `ignore_missing` | `false` | Don't error on missing files |
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::WorkflowOptions;
///
/// let opts = WorkflowOptions::new()
///     .with_prefix("src/lib/")
///     .with_check_mtime(false);
///
/// assert_eq!(opts.prefix(), "src/lib/");
/// assert!(!opts.check_mtime());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowOptions {
    /// Diff algorithm to use for content comparison.
    algorithm: Algorithm,

    /// Whether to use mtime to skip unchanged files.
    ///
    /// When `true`, files whose modification time hasn't changed since
    /// the last record are assumed unchanged and skipped.
    check_mtime: bool,

    /// Whether to detect moved files.
    ///
    /// Move detection requires computing content hashes for added and
    /// deleted files, then matching them. This can be expensive.
    detect_moves: bool,

    /// Whether to detect encoding changes (UTF-8 ↔ Binary).
    detect_encoding: bool,

    /// Whether to detect permission changes.
    detect_permissions: bool,

    /// Path prefix to filter detection/recording.
    ///
    /// Only files under this prefix will be processed. Empty string
    /// means all files.
    prefix: String,

    /// Maximum file size for text diffing (in bytes).
    ///
    /// Files larger than this are treated as binary (no diff ops).
    /// Set to `None` for no limit.
    max_file_size: Option<u64>,

    /// Force re-diffing even if file appears unchanged.
    ///
    /// Bypasses mtime and other optimizations.
    force_rediff: bool,

    /// Don't error on missing files during recording.
    ///
    /// When `true`, missing files are skipped instead of causing errors.
    ignore_missing: bool,
}

impl WorkflowOptions {
    /// Default maximum file size for diffing (10 MB).
    pub const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

    /// Create new options with default values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::WorkflowOptions;
    ///
    /// let opts = WorkflowOptions::new();
    /// assert!(opts.check_mtime());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - Algorithm to use for content comparison
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::WorkflowOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let opts = WorkflowOptions::new().with_algorithm(Algorithm::Patience);
    /// assert_eq!(opts.algorithm(), Algorithm::Patience);
    /// ```
    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set whether to use mtime optimization.
    ///
    /// # Arguments
    ///
    /// * `check` - Whether to check mtime before comparing content
    pub fn with_check_mtime(mut self, check: bool) -> Self {
        self.check_mtime = check;
        self
    }

    /// Set whether to detect moved files.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to detect file moves
    pub fn with_detect_moves(mut self, detect: bool) -> Self {
        self.detect_moves = detect;
        self
    }

    /// Set whether to detect encoding changes.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to track encoding changes
    pub fn with_detect_encoding(mut self, detect: bool) -> Self {
        self.detect_encoding = detect;
        self
    }

    /// Set whether to detect permission changes.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to track permission changes
    pub fn with_detect_permissions(mut self, detect: bool) -> Self {
        self.detect_permissions = detect;
        self
    }

    /// Set the path prefix filter.
    ///
    /// Only files under this prefix will be processed.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix (e.g., "src/", "tests/")
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::WorkflowOptions;
    ///
    /// let opts = WorkflowOptions::new().with_prefix("src/");
    /// assert!(opts.matches_prefix("src/main.rs"));
    /// assert!(!opts.matches_prefix("tests/test.rs"));
    /// ```
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Set the maximum file size for diffing.
    ///
    /// Files larger than this are treated as binary.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes, or `None` for no limit
    pub fn with_max_file_size(mut self, size: impl Into<Option<u64>>) -> Self {
        self.max_file_size = size.into();
        self
    }

    /// Set whether to force re-diffing.
    ///
    /// # Arguments
    ///
    /// * `force` - Whether to bypass optimizations
    pub fn with_force_rediff(mut self, force: bool) -> Self {
        self.force_rediff = force;
        self
    }

    /// Set whether to ignore missing files.
    ///
    /// # Arguments
    ///
    /// * `ignore` - Whether to skip missing files without error
    pub fn with_ignore_missing(mut self, ignore: bool) -> Self {
        self.ignore_missing = ignore;
        self
    }

    // Getters

    /// Get the diff algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Check if mtime optimization is enabled.
    pub fn check_mtime(&self) -> bool {
        self.check_mtime
    }

    /// Check if move detection is enabled.
    pub fn detect_moves(&self) -> bool {
        self.detect_moves
    }

    /// Check if encoding change detection is enabled.
    pub fn detect_encoding(&self) -> bool {
        self.detect_encoding
    }

    /// Check if permission change detection is enabled.
    pub fn detect_permissions(&self) -> bool {
        self.detect_permissions
    }

    /// Get the path prefix filter.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Get the maximum file size for diffing.
    pub fn max_file_size(&self) -> Option<u64> {
        self.max_file_size
    }

    /// Check if force rediff is enabled.
    pub fn force_rediff(&self) -> bool {
        self.force_rediff
    }

    /// Check if missing files should be ignored.
    pub fn ignore_missing(&self) -> bool {
        self.ignore_missing
    }

    // Helper Methods

    /// Check if a path matches the prefix filter.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Returns
    ///
    /// `true` if the path matches (or prefix is empty).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::WorkflowOptions;
    ///
    /// let opts = WorkflowOptions::new().with_prefix("src/");
    ///
    /// assert!(opts.matches_prefix("src/main.rs"));
    /// assert!(opts.matches_prefix("src/lib/mod.rs"));
    /// assert!(!opts.matches_prefix("tests/test.rs"));
    ///
    /// // Empty prefix matches everything
    /// let opts = WorkflowOptions::new();
    /// assert!(opts.matches_prefix("anything"));
    /// ```
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            return true;
        }
        path.starts_with(&self.prefix)
    }

    /// Check if a file size exceeds the maximum for diffing.
    ///
    /// # Arguments
    ///
    /// * `size` - File size in bytes
    ///
    /// # Returns
    ///
    /// `true` if the file is too large for text diffing.
    pub fn exceeds_max_size(&self, size: u64) -> bool {
        match self.max_file_size {
            Some(max) => size > max,
            None => false,
        }
    }

    /// Check if content should be diffed based on current options.
    ///
    /// This considers force_rediff and other flags.
    ///
    /// # Arguments
    ///
    /// * `mtime_unchanged` - Whether the file's mtime is unchanged
    ///
    /// # Returns
    ///
    /// `true` if the file should be diffed.
    pub fn should_diff(&self, mtime_unchanged: bool) -> bool {
        if self.force_rediff {
            return true;
        }
        if self.check_mtime && mtime_unchanged {
            return false;
        }
        true
    }
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            check_mtime: true,
            detect_moves: true,
            detect_encoding: true,
            detect_permissions: true,
            prefix: String::new(),
            max_file_size: Some(Self::DEFAULT_MAX_FILE_SIZE),
            force_rediff: false,
            ignore_missing: false,
        }
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // Construction Tests

    #[test]
    fn test_new_returns_default() {
        let opts = WorkflowOptions::new();
        let default = WorkflowOptions::default();

        assert_eq!(opts, default);
    }

    #[test]
    fn test_default_values() {
        let opts = WorkflowOptions::default();

        assert_eq!(opts.algorithm(), Algorithm::Myers);
        assert!(opts.check_mtime());
        assert!(opts.detect_moves());
        assert!(opts.detect_encoding());
        assert!(opts.detect_permissions());
        assert!(opts.prefix().is_empty());
        assert_eq!(opts.with_max_file_size(), Some(WorkflowOptions::DEFAULT_MAX_FILE_SIZE));
        assert!(!opts.force_rediff());
        assert!(!opts.ignore_missing());
    }

    // Builder Tests

    #[test]
    fn test_algorithm_builder() {
        let opts = WorkflowOptions::new().with_algorithm(Algorithm::Patience);
        assert_eq!(opts.algorithm(), Algorithm::Patience);
    }

    #[test]
    fn test_check_mtime_builder() {
        let opts = WorkflowOptions::new().with_check_mtime(false);
        assert!(!opts.check_mtime());
    }

    #[test]
    fn test_detect_moves_builder() {
        let opts = WorkflowOptions::new().with_detect_moves(false);
        assert!(!opts.detect_moves());
    }

    #[test]
    fn test_detect_encoding_builder() {
        let opts = WorkflowOptions::new().with_detect_encoding(false);
        assert!(!opts.detect_encoding());
    }

    #[test]
    fn test_detect_permissions_builder() {
        let opts = WorkflowOptions::new().with_detect_permissions(false);
        assert!(!opts.detect_permissions());
    }

    #[test]
    fn test_prefix_builder_str() {
        let opts = WorkflowOptions::new().with_prefix("src/");
        assert_eq!(opts.prefix(), "src/");
    }

    #[test]
    fn test_prefix_builder_string() {
        let opts = WorkflowOptions::new().prefix(String::from("tests/"));
        assert_eq!(opts.prefix(), "tests/");
    }

    #[test]
    fn test_max_file_size_builder_some() {
        let opts = WorkflowOptions::new().with_max_file_size(1024u64);
        assert_eq!(opts.with_max_file_size(), Some(1024));
    }

    #[test]
    fn test_max_file_size_builder_none() {
        let opts = WorkflowOptions::new().with_max_file_size(None);
        assert_eq!(opts.with_max_file_size(), None);
    }

    #[test]
    fn test_force_rediff_builder() {
        let opts = WorkflowOptions::new().with_force_rediff(true);
        assert!(opts.force_rediff());
    }

    #[test]
    fn test_ignore_missing_builder() {
        let opts = WorkflowOptions::new().with_ignore_missing(true);
        assert!(opts.ignore_missing());
    }

    #[test]
    fn test_builder_chaining() {
        let opts = WorkflowOptions::new()
            .with_algorithm(Algorithm::Patience)
            .with_check_mtime(false)
            .with_detect_moves(false)
            .with_prefix("src/")
            .with_max_file_size(1024u64)
            .with_force_rediff(true)
            .with_ignore_missing(true);

        assert_eq!(opts.algorithm(), Algorithm::Patience);
        assert!(!opts.check_mtime());
        assert!(!opts.detect_moves());
        assert_eq!(opts.prefix(), "src/");
        assert_eq!(opts.with_max_file_size(), Some(1024));
        assert!(opts.force_rediff());
        assert!(opts.ignore_missing());
    }

    // Helper Method Tests

    #[test]
    fn test_matches_prefix_empty() {
        let opts = WorkflowOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_matches_prefix_with_prefix() {
        let opts = WorkflowOptions::new().with_prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(opts.matches_prefix("src/"));
        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
        assert!(!opts.matches_prefix(""));
    }

    #[test]
    fn test_matches_prefix_exact() {
        let opts = WorkflowOptions::new().with_prefix("src/main.rs");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(!opts.matches_prefix("src/lib.rs"));
    }

    #[test]
    fn test_exceeds_max_size_with_limit() {
        let opts = WorkflowOptions::new().with_max_file_size(1024u64);

        assert!(!opts.exceeds_max_size(1024));
        assert!(!opts.exceeds_max_size(100));
        assert!(opts.exceeds_max_size(1025));
        assert!(opts.exceeds_max_size(10000));
    }

    #[test]
    fn test_exceeds_max_size_no_limit() {
        let opts = WorkflowOptions::new().with_max_file_size(None);

        assert!(!opts.exceeds_max_size(0));
        assert!(!opts.exceeds_max_size(u64::MAX));
    }

    #[test]
    fn test_should_diff_default() {
        let opts = WorkflowOptions::new();

        // mtime unchanged - should NOT diff (optimization)
        assert!(!opts.should_diff(true));

        // mtime changed - should diff
        assert!(opts.should_diff(false));
    }

    #[test]
    fn test_should_diff_force_rediff() {
        let opts = WorkflowOptions::new().with_force_rediff(true);

        // Even if mtime unchanged, should diff
        assert!(opts.should_diff(true));
        assert!(opts.should_diff(false));
    }

    #[test]
    fn test_should_diff_mtime_disabled() {
        let opts = WorkflowOptions::new().with_check_mtime(false);

        // mtime checking disabled - always diff
        assert!(opts.should_diff(true));
        assert!(opts.should_diff(false));
    }

    // Clone and Debug Tests

    #[test]
    fn test_clone() {
        let opts = WorkflowOptions::new()
            .with_prefix("test/")
            .with_algorithm(Algorithm::Patience);

        let cloned = opts.clone();

        assert_eq!(cloned.prefix(), "test/");
        assert_eq!(cloned.algorithm(), Algorithm::Patience);
    }

    #[test]
    fn test_debug() {
        let opts = WorkflowOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("WorkflowOptions"));
        assert!(debug.contains("algorithm"));
    }

    #[test]
    fn test_eq() {
        let opts1 = WorkflowOptions::new().with_prefix("src/");
        let opts2 = WorkflowOptions::new().with_prefix("src/");
        let opts3 = WorkflowOptions::new().with_prefix("tests/");

        assert_eq!(opts1, opts2);
        assert_ne!(opts1, opts3);
    }
}

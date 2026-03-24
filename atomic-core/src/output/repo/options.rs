//! Output options configuration.
//!
//! This module provides the [`OutputOptions`] struct for configuring how
//! repository state is output to the working copy.
//!
//! # Overview
//!
//! Output options control various aspects of the output process:
//!
//! - **Filtering**: Which files to output (by path prefix, modification time)
//! - **Conflict handling**: How to handle name conflicts
//! - **Safety limits**: Maximum vertices to process per file
//! - **Determinism**: Salt for reproducible conflict naming
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::repo::OutputOptions;
//! use std::time::SystemTime;
//!
//! // Create options with all defaults
//! let opts = OutputOptions::default();
//!
//! // Or use the builder pattern
//! let opts = OutputOptions::new()
//!     .with_prefix("src/")
//!     .output_name_conflicts(true)
//!     .if_modified_since(Some(SystemTime::now()))
//!     .max_vertices(100_000)
//!     .salt(42);
//! ```
//!
//! # Design Notes
//!
//! Options use the builder pattern for ergonomic construction. All fields
//! have sensible defaults, so you can just use `OutputOptions::default()`
//! for most cases.

use std::time::SystemTime;

// OUTPUT OPTIONS

/// Configuration options for repository output operations.
///
/// This struct controls how the repository graph state is written to the
/// working copy. All fields have sensible defaults.
///
/// # Fields
///
/// | Field | Default | Description |
/// |-------|---------|-------------|
/// | `output_name_conflicts` | `true` | Create separate files for name conflicts |
/// | `if_modified_since` | `None` | Skip files not modified after this time |
/// | `prefix` | `""` | Only output files under this path prefix |
/// | `include_deleted` | `false` | Include deleted content in output |
/// | `max_vertices` | `None` | Safety limit on vertices per file |
/// | `salt` | `0` | Deterministic salt for conflict naming |
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::OutputOptions;
///
/// let opts = OutputOptions::new()
///     .with_prefix("src/lib/")
///     .include_deleted(false)
///     .max_vertices(50_000);
///
/// assert_eq!(opts.prefix, "src/lib/");
/// assert!(!opts.include_deleted);
/// assert_eq!(opts.max_vertices, Some(50_000));
/// ```
#[derive(Debug, Clone)]
pub struct OutputOptions {
    /// Whether to output files with name conflicts as separate files.
    ///
    /// When `true`, if multiple changes assign different names to the same
    /// inode, each name is created with a disambiguating suffix (e.g.,
    /// `filename.CHANGEHASH`).
    ///
    /// When `false`, only the first name encountered is used.
    ///
    /// Default: `true`
    pub output_name_conflicts: bool,

    /// Only output files modified after this time.
    ///
    /// If set, files whose modification time in the working copy is before
    /// this time will be skipped. This enables incremental output after
    /// pulling changes.
    ///
    /// Default: `None` (output all files)
    pub if_modified_since: Option<SystemTime>,

    /// Path prefix filter for output.
    ///
    /// If non-empty, only files whose paths start with this prefix will
    /// be output. This allows outputting a subtree of the repository.
    ///
    /// Default: `""` (output entire repository)
    pub prefix: String,

    /// Whether to include deleted content in conflict output.
    ///
    /// When `true`, content that has been deleted but is still referenced
    /// (zombie content) will be included in the output with appropriate
    /// conflict markers.
    ///
    /// Default: `false`
    pub include_deleted: bool,

    /// Maximum number of vertices to process per file.
    ///
    /// This is a safety limit to prevent runaway processing on malformed
    /// or extremely large graphs. If a file exceeds this limit, output
    /// will be truncated.
    ///
    /// Default: `None` (no limit)
    pub max_vertices: Option<usize>,

    /// Salt for deterministic conflict naming.
    ///
    /// Used when generating unique names for conflict files. Using the
    /// same salt produces the same names across runs, which is useful
    /// for testing and reproducibility.
    ///
    /// Default: `0`
    pub salt: u64,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            output_name_conflicts: true,
            if_modified_since: None,
            prefix: String::new(),
            include_deleted: false,
            max_vertices: None,
            salt: 0,
        }
    }
}

impl OutputOptions {
    /// Create new options with default values.
    ///
    /// This is equivalent to `OutputOptions::default()` but reads better
    /// when chaining builder methods.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new()
    ///     .with_prefix("src/")
    ///     .salt(123);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to output name conflicts as separate files.
    ///
    /// # Arguments
    ///
    /// * `output` - If `true`, create disambiguated files for name conflicts
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().output_name_conflicts(false);
    /// assert!(!opts.output_name_conflicts);
    /// ```
    #[must_use]
    pub fn output_name_conflicts(mut self, output: bool) -> Self {
        self.output_name_conflicts = output;
        self
    }

    /// Set the modification time threshold for incremental output.
    ///
    /// Files not modified after this time will be skipped.
    ///
    /// # Arguments
    ///
    /// * `time` - The threshold time, or `None` to output all files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    /// use std::time::SystemTime;
    ///
    /// let now = SystemTime::now();
    /// let opts = OutputOptions::new().if_modified_since(Some(now));
    /// assert!(opts.if_modified_since.is_some());
    /// ```
    #[must_use]
    pub fn if_modified_since(mut self, time: Option<SystemTime>) -> Self {
        self.if_modified_since = time;
        self
    }

    /// Set the path prefix filter.
    ///
    /// Only files whose paths start with this prefix will be output.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The path prefix (e.g., `"src/"` or `"tests/integration/"`)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().with_prefix("src/utils/");
    /// assert_eq!(opts.prefix, "src/utils/");
    /// ```
    #[must_use]
    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    /// Set whether to include deleted content.
    ///
    /// # Arguments
    ///
    /// * `include` - If `true`, include zombie content in output
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().include_deleted(true);
    /// assert!(opts.include_deleted);
    /// ```
    #[must_use]
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum vertices per file.
    ///
    /// # Arguments
    ///
    /// * `max` - The maximum number of vertices to process
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().max_vertices(100_000);
    /// assert_eq!(opts.max_vertices, Some(100_000));
    /// ```
    #[must_use]
    pub fn max_vertices(mut self, max: usize) -> Self {
        self.max_vertices = Some(max);
        self
    }

    /// Clear the maximum vertices limit.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new()
    ///     .max_vertices(1000)
    ///     .no_vertex_limit();
    /// assert!(opts.max_vertices.is_none());
    /// ```
    #[must_use]
    pub fn no_vertex_limit(mut self) -> Self {
        self.max_vertices = None;
        self
    }

    /// Set the salt for deterministic conflict naming.
    ///
    /// # Arguments
    ///
    /// * `salt` - The salt value
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().salt(42);
    /// assert_eq!(opts.salt, 42);
    /// ```
    #[must_use]
    pub fn salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// Check if a path matches the prefix filter.
    ///
    /// Returns `true` if the path should be included based on the prefix.
    /// An empty prefix matches all paths.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to check
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    ///
    /// let opts = OutputOptions::new().with_prefix("src/");
    ///
    /// assert!(opts.matches_prefix("src/main.rs"));
    /// assert!(opts.matches_prefix("src/lib/utils.rs"));
    /// assert!(!opts.matches_prefix("tests/test.rs"));
    ///
    /// // Empty prefix matches everything
    /// let opts = OutputOptions::new();
    /// assert!(opts.matches_prefix("anything/goes.rs"));
    /// ```
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            return true;
        }
        path.starts_with(&self.prefix)
    }

    /// Check if we should output based on modification time.
    ///
    /// Returns `true` if the file should be output based on its modification
    /// time and the `if_modified_since` threshold.
    ///
    /// # Arguments
    ///
    /// * `mtime` - The file's modification time
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOptions;
    /// use std::time::{SystemTime, Duration};
    ///
    /// let threshold = SystemTime::now();
    /// let opts = OutputOptions::new().if_modified_since(Some(threshold));
    ///
    /// // Files modified after threshold should be output
    /// let later = threshold + Duration::from_secs(10);
    /// assert!(opts.should_output_by_time(later));
    ///
    /// // Files modified before threshold should be skipped
    /// let earlier = threshold - Duration::from_secs(10);
    /// assert!(!opts.should_output_by_time(earlier));
    /// ```
    pub fn should_output_by_time(&self, mtime: SystemTime) -> bool {
        match self.if_modified_since {
            Some(threshold) => mtime >= threshold,
            None => true,
        }
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ------------------------------------------------------------------------
    // Default Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_default_values() {
        let opts = OutputOptions::default();

        assert!(opts.output_name_conflicts);
        assert!(opts.if_modified_since.is_none());
        assert!(opts.prefix.is_empty());
        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
        assert_eq!(opts.salt, 0);
    }

    #[test]
    fn test_new_equals_default() {
        let new_opts = OutputOptions::new();
        let default_opts = OutputOptions::default();

        assert_eq!(
            new_opts.output_name_conflicts,
            default_opts.output_name_conflicts
        );
        assert_eq!(new_opts.if_modified_since, default_opts.if_modified_since);
        assert_eq!(new_opts.prefix, default_opts.prefix);
        assert_eq!(new_opts.include_deleted, default_opts.include_deleted);
        assert_eq!(new_opts.max_vertices, default_opts.max_vertices);
        assert_eq!(new_opts.salt, default_opts.salt);
    }

    // ------------------------------------------------------------------------
    // Builder Pattern Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_builder_output_name_conflicts() {
        let opts = OutputOptions::new().output_name_conflicts(false);
        assert!(!opts.output_name_conflicts);

        let opts = OutputOptions::new().output_name_conflicts(true);
        assert!(opts.output_name_conflicts);
    }

    #[test]
    fn test_builder_if_modified_since() {
        let now = SystemTime::now();
        let opts = OutputOptions::new().if_modified_since(Some(now));

        assert_eq!(opts.if_modified_since, Some(now));

        let opts = OutputOptions::new().if_modified_since(None);
        assert!(opts.if_modified_since.is_none());
    }

    #[test]
    fn test_builder_prefix() {
        let opts = OutputOptions::new().with_prefix("src/");
        assert_eq!(opts.prefix, "src/");

        let opts = OutputOptions::new().with_prefix("");
        assert!(opts.prefix.is_empty());

        let opts = OutputOptions::new().with_prefix("deeply/nested/path/");
        assert_eq!(opts.prefix, "deeply/nested/path/");
    }

    #[test]
    fn test_builder_include_deleted() {
        let opts = OutputOptions::new().include_deleted(true);
        assert!(opts.include_deleted);

        let opts = OutputOptions::new().include_deleted(false);
        assert!(!opts.include_deleted);
    }

    #[test]
    fn test_builder_max_vertices() {
        let opts = OutputOptions::new().max_vertices(1000);
        assert_eq!(opts.max_vertices, Some(1000));

        let opts = OutputOptions::new().max_vertices(0);
        assert_eq!(opts.max_vertices, Some(0));

        let opts = OutputOptions::new().max_vertices(usize::MAX);
        assert_eq!(opts.max_vertices, Some(usize::MAX));
    }

    #[test]
    fn test_builder_no_vertex_limit() {
        let opts = OutputOptions::new().max_vertices(1000).no_vertex_limit();
        assert!(opts.max_vertices.is_none());
    }

    #[test]
    fn test_builder_salt() {
        let opts = OutputOptions::new().salt(42);
        assert_eq!(opts.salt, 42);

        let opts = OutputOptions::new().salt(u64::MAX);
        assert_eq!(opts.salt, u64::MAX);
    }

    #[test]
    fn test_builder_chaining() {
        let now = SystemTime::now();
        let opts = OutputOptions::new()
            .with_prefix("src/")
            .output_name_conflicts(false)
            .if_modified_since(Some(now))
            .include_deleted(true)
            .max_vertices(50_000)
            .salt(123);

        assert_eq!(opts.prefix, "src/");
        assert!(!opts.output_name_conflicts);
        assert_eq!(opts.if_modified_since, Some(now));
        assert!(opts.include_deleted);
        assert_eq!(opts.max_vertices, Some(50_000));
        assert_eq!(opts.salt, 123);
    }

    // ------------------------------------------------------------------------
    // matches_prefix Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_matches_prefix_empty() {
        let opts = OutputOptions::new();

        assert!(opts.matches_prefix(""));
        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("deeply/nested/path/file.txt"));
    }

    #[test]
    fn test_matches_prefix_with_prefix() {
        let opts = OutputOptions::new().with_prefix("src/");

        assert!(opts.matches_prefix("src/"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));

        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
        assert!(!opts.matches_prefix(""));
    }

    #[test]
    fn test_matches_prefix_exact_match() {
        let opts = OutputOptions::new().with_prefix("src/main.rs");

        assert!(opts.matches_prefix("src/main.rs"));
        // Note: "src/main.rs.bak" DOES match because it starts with "src/main.rs"
        // This is the expected behavior for prefix matching
        assert!(opts.matches_prefix("src/main.rs.bak"));
        assert!(!opts.matches_prefix("src/lib.rs"));
    }

    // ------------------------------------------------------------------------
    // should_output_by_time Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_should_output_by_time_no_threshold() {
        let opts = OutputOptions::new();
        let any_time = SystemTime::UNIX_EPOCH;

        assert!(opts.should_output_by_time(any_time));
        assert!(opts.should_output_by_time(SystemTime::now()));
    }

    #[test]
    fn test_should_output_by_time_with_threshold() {
        let threshold = SystemTime::now();
        let opts = OutputOptions::new().if_modified_since(Some(threshold));

        // Files modified at or after threshold should be output
        assert!(opts.should_output_by_time(threshold));

        let later = threshold + Duration::from_secs(100);
        assert!(opts.should_output_by_time(later));

        // Files modified before threshold should be skipped
        let earlier = threshold - Duration::from_secs(100);
        assert!(!opts.should_output_by_time(earlier));
    }

    // ------------------------------------------------------------------------
    // Clone and Debug Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_clone() {
        let opts = OutputOptions::new().with_prefix("src/").salt(42);

        let cloned = opts.clone();

        assert_eq!(cloned.prefix, opts.prefix);
        assert_eq!(cloned.salt, opts.salt);
    }

    #[test]
    fn test_debug() {
        let opts = OutputOptions::new().with_prefix("test/");
        let debug_str = format!("{:?}", opts);

        assert!(debug_str.contains("OutputOptions"));
        assert!(debug_str.contains("test/"));
    }
}

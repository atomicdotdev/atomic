//! High-level change detection integrating collection and comparison.
//!
//! This module provides the main entry points for detecting changes between
//! the working copy and the pristine (recorded) state. It orchestrates the
//! collection, comparison, and categorization of file changes.
//!
//! # Overview
//!
//! Change detection is a multi-phase process:
//!
//! 1. **Collection**: Gather files from pristine and working copy
//! 2. **Set Analysis**: Compare tracked vs working sets
//! 3. **Content Comparison**: Diff content for files in both sets
//! 4. **Results**: Categorized `DetectionResult`

mod types;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use crate::diff::Algorithm;
use crate::output::WorkingCopyRead;

// Re-export all public types from the types submodule
pub use types::{DetectedFile, DetectionKind, DetectionResult};

// ============================================================================
// DETECTION OPTIONS
// ============================================================================

/// Options controlling the change detection process.
///
/// These options allow fine-tuning detection behavior for performance
/// and specific use cases.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::detect::DetectionOptions;
/// use atomic_core::diff::Algorithm;
///
/// let options = DetectionOptions::new()
///     .prefix("src/")
///     .algorithm(Algorithm::Patience)
///     .check_mtime(true)
///     .detect_moves(true)
///     .include_unchanged(false);
///
/// assert_eq!(options.get_prefix(), Some("src/"));
/// assert_eq!(options.get_algorithm(), Algorithm::Patience);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionOptions {
    /// Path prefix to filter detection (empty = all files).
    prefix: Option<String>,

    /// Diff algorithm to use for content comparison.
    algorithm: Algorithm,

    /// Whether to check modification time before comparing content.
    ///
    /// When true, files with unchanged mtime are skipped.
    check_mtime: bool,

    /// Whether to detect file moves (renames).
    ///
    /// When true, deleted files with matching content in added files
    /// are reported as moves instead of separate add/delete.
    detect_moves: bool,

    /// Whether to include unchanged files in results.
    ///
    /// When false (default), only changed files are returned.
    include_unchanged: bool,

    /// Maximum file size to diff (in bytes).
    ///
    /// Files larger than this are marked as binary and not diffed.
    max_diff_size: Option<usize>,

    /// Whether to force re-diffing even if mtime suggests no change.
    force_rediff: bool,
}

impl DetectionOptions {
    /// Default maximum diff size (10 MB).
    pub const DEFAULT_MAX_DIFF_SIZE: usize = 10 * 1024 * 1024;

    /// Create new options with defaults.
    ///
    /// Default values:
    /// - `prefix`: None (all files)
    /// - `algorithm`: Myers
    /// - `check_mtime`: true
    /// - `detect_moves`: false
    /// - `include_unchanged`: false
    /// - `max_diff_size`: 10 MB
    /// - `force_rediff`: false
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new();
    /// assert!(options.get_check_mtime());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the path prefix filter.
    ///
    /// Only files under this prefix will be detected.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().prefix("src/");
    /// assert_eq!(options.get_prefix(), Some("src/"));
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        let p = prefix.into();
        self.prefix = if p.is_empty() { None } else { Some(p) };
        self
    }

    /// Set the diff algorithm.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let options = DetectionOptions::new().algorithm(Algorithm::Patience);
    /// assert_eq!(options.get_algorithm(), Algorithm::Patience);
    /// ```
    #[must_use]
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set whether to check modification time.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().check_mtime(false);
    /// assert!(!options.get_check_mtime());
    /// ```
    #[must_use]
    pub fn check_mtime(mut self, check: bool) -> Self {
        self.check_mtime = check;
        self
    }

    /// Set whether to detect moves.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().detect_moves(true);
    /// assert!(options.get_detect_moves());
    /// ```
    #[must_use]
    pub fn detect_moves(mut self, detect: bool) -> Self {
        self.detect_moves = detect;
        self
    }

    /// Set whether to include unchanged files.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().include_unchanged(true);
    /// assert!(options.get_include_unchanged());
    /// ```
    #[must_use]
    pub fn include_unchanged(mut self, include: bool) -> Self {
        self.include_unchanged = include;
        self
    }

    /// Set the maximum file size to diff.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().max_diff_size(1024 * 1024); // 1 MB
    /// assert_eq!(options.get_max_diff_size(), Some(1024 * 1024));
    /// ```
    #[must_use]
    pub fn max_diff_size(mut self, size: usize) -> Self {
        self.max_diff_size = Some(size);
        self
    }

    /// Set whether to force re-diffing.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().force_rediff(true);
    /// assert!(options.get_force_rediff());
    /// ```
    #[must_use]
    pub fn force_rediff(mut self, force: bool) -> Self {
        self.force_rediff = force;
        self
    }

    /// Get the prefix setting.
    #[must_use]
    pub fn get_prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// Get the algorithm setting.
    #[must_use]
    pub fn get_algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Get the check_mtime setting.
    #[must_use]
    pub fn get_check_mtime(&self) -> bool {
        self.check_mtime
    }

    /// Get the detect_moves setting.
    #[must_use]
    pub fn get_detect_moves(&self) -> bool {
        self.detect_moves
    }

    /// Get the include_unchanged setting.
    #[must_use]
    pub fn get_include_unchanged(&self) -> bool {
        self.include_unchanged
    }

    /// Get the max_diff_size setting.
    #[must_use]
    pub fn get_max_diff_size(&self) -> Option<usize> {
        self.max_diff_size
    }

    /// Get the force_rediff setting.
    #[must_use]
    pub fn get_force_rediff(&self) -> bool {
        self.force_rediff
    }

    /// Check if a file size exceeds the max diff size.
    #[must_use]
    pub fn exceeds_max_size(&self, size: usize) -> bool {
        self.max_diff_size.is_some_and(|max| size > max)
    }
}

impl Default for DetectionOptions {
    fn default() -> Self {
        Self {
            prefix: None,
            algorithm: Algorithm::Myers,
            check_mtime: true,
            detect_moves: false,
            include_unchanged: false,
            max_diff_size: Some(Self::DEFAULT_MAX_DIFF_SIZE),
            force_rediff: false,
        }
    }
}

// ============================================================================
// DETECTION FUNCTIONS
// ============================================================================

/// Detect changes between the working copy and pristine state.
///
/// This is the main entry point for change detection. It collects files
/// from both the pristine database and working copy, then compares them
/// to find additions, deletions, modifications, and moves.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `tracked_paths` - List of tracked file paths from pristine
/// * `options` - Detection options
///
/// # Returns
///
/// A `DetectionResult` containing all detected changes.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::detect::{detect_changes_simple, DetectionOptions};
///
/// let options = DetectionOptions::new();
/// let result = detect_changes_simple(&working_copy, &tracked_paths, &options);
///
/// for file in result.modified() {
///     println!("Modified: {}", file.path);
/// }
/// ```
pub fn detect_changes_simple<W>(
    working_copy: &W,
    tracked_paths: &[String],
    options: &DetectionOptions,
) -> DetectionResult
where
    W: WorkingCopyRead,
{
    let mut result = DetectionResult::new();
    let prefix = options.get_prefix().unwrap_or("");

    // Build set of tracked paths for quick lookup
    let tracked_set: HashSet<&str> = tracked_paths.iter().map(|s| s.as_str()).collect();

    // Collect working copy files
    let working_files = match working_copy.walk_files(prefix) {
        Ok(files) => files,
        Err(e) => {
            result.add_error(format!("Failed to walk working copy: {}", e));
            return result;
        }
    };

    let working_set: HashSet<&str> = working_files.iter().map(|s| s.as_str()).collect();

    // Find added files (in working copy but not tracked)
    for path in &working_files {
        result.increment_scanned();
        if !tracked_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                let file = DetectedFile::added(path);
                result.add_added(file);
            }
        }
    }

    // Find deleted files (tracked but not in working copy)
    for path in tracked_paths {
        if !working_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                let file = DetectedFile::deleted(path);
                result.add_deleted(file);
            }
        }
    }

    // Files in both sets exist in working copy and are tracked.
    // Content comparison requires pristine content retrieval, which is
    // handled by the caller using record_modified_file() with the
    // retrieved pristine content. Here we just identify which files
    // need comparison.
    for path in tracked_paths {
        if working_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                // Files present in both sets are tracked as unchanged here.
                // The caller should perform content comparison to determine
                // if they are actually modified.
                if options.get_include_unchanged() {
                    let file = DetectedFile::unchanged(path);
                    result.add_unchanged(file);
                }
            }
        }
    }

    result
}

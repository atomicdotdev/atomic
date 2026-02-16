//! Output outcome tracking.
//!
//! This module provides the [`OutputOutcome`] struct for tracking the results
//! of repository output operations, including statistics and any conflicts
//! that were detected.
//!
//! # Overview
//!
//! After outputting repository state to the working copy, you need to know:
//!
//! - How many files were written
//! - How many directories were created
//! - How many files were skipped (unchanged)
//! - Whether any conflicts were detected
//! - Total bytes written
//!
//! The `OutputOutcome` struct captures all this information.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::repo::OutputOutcome;
//!
//! let mut outcome = OutputOutcome::new();
//!
//! // Record operations as they happen
//! outcome.record_file("src/main.rs", 1024);
//! outcome.record_file("src/lib.rs", 512);
//! outcome.record_directory("src/utils");
//! outcome.record_skip("README.md");
//!
//! // Check results
//! assert_eq!(outcome.files_written(), 2);
//! assert_eq!(outcome.directories_created(), 1);
//! assert_eq!(outcome.files_skipped(), 1);
//! assert_eq!(outcome.bytes_written, 1536);
//! ```

use std::collections::HashSet;

// OUTPUT OUTCOME

/// The result of outputting repository state to the working copy.
///
/// This struct tracks statistics about what was written during an output
/// operation. It records:
///
/// - Files written (with byte counts)
/// - Directories created
/// - Files skipped (because they were unchanged or filtered)
/// - Total bytes written
/// - Redundant edges found (for potential cleanup)
///
/// # Thread Safety
///
/// This struct is not thread-safe. If you need to collect outcomes from
/// multiple threads, collect them separately and then merge.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::OutputOutcome;
///
/// let mut outcome = OutputOutcome::new();
///
/// outcome.record_file("main.rs", 100);
/// outcome.record_directory("src");
///
/// assert_eq!(outcome.files_written(), 1);
/// assert_eq!(outcome.directories_created(), 1);
/// assert!(!outcome.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct OutputOutcome {
    /// Paths of files that were written, with their byte counts.
    files: Vec<FileWritten>,

    /// Paths of directories that were created.
    directories: HashSet<String>,

    /// Paths of files that were skipped (unchanged or filtered).
    skipped: Vec<String>,

    /// Total bytes written across all files.
    pub bytes_written: u64,

    /// Number of redundant edges detected during output.
    ///
    /// Redundant edges are forward edges that could be removed from the
    /// graph without changing the semantics. This is informational only.
    pub redundant_edges: usize,
}

/// A file that was written during output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWritten {
    /// Path to the file.
    pub path: String,

    /// Number of bytes written.
    pub bytes: u64,
}

impl FileWritten {
    /// Create a new file written record.
    pub fn new(path: impl Into<String>, bytes: u64) -> Self {
        Self {
            path: path.into(),
            bytes,
        }
    }
}

impl OutputOutcome {
    /// Create a new empty outcome.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let outcome = OutputOutcome::new();
    /// assert!(outcome.is_empty());
    /// assert_eq!(outcome.bytes_written, 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if no operations were performed.
    ///
    /// Returns `true` if no files were written, no directories created,
    /// and no files skipped.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// assert!(outcome.is_empty());
    ///
    /// outcome.record_file("test.rs", 100);
    /// assert!(!outcome.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.directories.is_empty() && self.skipped.is_empty()
    }

    /// Get the number of files written.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_file("a.rs", 10);
    /// outcome.record_file("b.rs", 20);
    ///
    /// assert_eq!(outcome.files_written(), 2);
    /// ```
    pub fn files_written(&self) -> usize {
        self.files.len()
    }

    /// Get the number of directories created.
    pub fn directories_created(&self) -> usize {
        self.directories.len()
    }

    /// Get the number of files skipped.
    pub fn files_skipped(&self) -> usize {
        self.skipped.len()
    }

    /// Get an iterator over files that were written.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_file("main.rs", 100);
    /// outcome.record_file("lib.rs", 200);
    ///
    /// let paths: Vec<_> = outcome.iter_files().map(|f| &f.path).collect();
    /// assert!(paths.contains(&&"main.rs".to_string()));
    /// assert!(paths.contains(&&"lib.rs".to_string()));
    /// ```
    pub fn iter_files(&self) -> impl Iterator<Item = &FileWritten> {
        self.files.iter()
    }

    /// Get an iterator over directories that were created.
    pub fn iter_directories(&self) -> impl Iterator<Item = &String> {
        self.directories.iter()
    }

    /// Get an iterator over files that were skipped.
    pub fn iter_skipped(&self) -> impl Iterator<Item = &String> {
        self.skipped.iter()
    }

    /// Record a file write.
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file that was written
    /// * `bytes` - The number of bytes written
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_file("src/main.rs", 1024);
    ///
    /// assert_eq!(outcome.files_written(), 1);
    /// assert_eq!(outcome.bytes_written, 1024);
    /// ```
    pub fn record_file(&mut self, path: impl Into<String>, bytes: u64) {
        self.files.push(FileWritten::new(path, bytes));
        self.bytes_written += bytes;
    }

    /// Record a directory creation.
    ///
    /// Duplicate directory paths are automatically deduplicated.
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the directory that was created
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_directory("src");
    /// outcome.record_directory("src/utils");
    /// outcome.record_directory("src"); // Duplicate, ignored
    ///
    /// assert_eq!(outcome.directories_created(), 2);
    /// ```
    pub fn record_directory(&mut self, path: impl Into<String>) {
        self.directories.insert(path.into());
    }

    /// Record a skipped file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file that was skipped
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_skip("unchanged.txt");
    ///
    /// assert_eq!(outcome.files_skipped(), 1);
    /// ```
    pub fn record_skip(&mut self, path: impl Into<String>) {
        self.skipped.push(path.into());
    }

    /// Record redundant edges found during output.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of redundant edges to add
    pub fn record_redundant_edges(&mut self, count: usize) {
        self.redundant_edges += count;
    }

    /// Merge another outcome into this one.
    ///
    /// This is useful when collecting results from multiple output operations
    /// (e.g., outputting different subtrees).
    ///
    /// # Arguments
    ///
    /// * `other` - The outcome to merge in
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome1 = OutputOutcome::new();
    /// outcome1.record_file("a.rs", 100);
    ///
    /// let mut outcome2 = OutputOutcome::new();
    /// outcome2.record_file("b.rs", 200);
    /// outcome2.record_directory("src");
    ///
    /// outcome1.merge(outcome2);
    ///
    /// assert_eq!(outcome1.files_written(), 2);
    /// assert_eq!(outcome1.directories_created(), 1);
    /// assert_eq!(outcome1.bytes_written, 300);
    /// ```
    pub fn merge(&mut self, other: OutputOutcome) {
        self.files.extend(other.files);
        self.directories.extend(other.directories);
        self.skipped.extend(other.skipped);
        self.bytes_written += other.bytes_written;
        self.redundant_edges += other.redundant_edges;
    }

    /// Create a summary string for display.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputOutcome;
    ///
    /// let mut outcome = OutputOutcome::new();
    /// outcome.record_file("main.rs", 1024);
    /// outcome.record_directory("src");
    ///
    /// let summary = outcome.summary();
    /// assert!(summary.contains("1 file"));
    /// assert!(summary.contains("1 director"));
    /// ```
    pub fn summary(&self) -> String {
        let files = self.files_written();
        let dirs = self.directories_created();
        let skipped = self.files_skipped();

        let mut parts = Vec::new();

        if files > 0 {
            let file_word = if files == 1 { "file" } else { "files" };
            parts.push(format!("{} {} written", files, file_word));
        }

        if dirs > 0 {
            let dir_word = if dirs == 1 { "directory" } else { "directories" };
            parts.push(format!("{} {} created", dirs, dir_word));
        }

        if skipped > 0 {
            let skip_word = if skipped == 1 { "file" } else { "files" };
            parts.push(format!("{} {} skipped", skipped, skip_word));
        }

        if parts.is_empty() {
            "No changes".to_string()
        } else {
            parts.join(", ")
        }
    }
}

impl std::fmt::Display for OutputOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.summary())
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Constructor Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_new_is_empty() {
        let outcome = OutputOutcome::new();

        assert!(outcome.is_empty());
        assert_eq!(outcome.files_written(), 0);
        assert_eq!(outcome.directories_created(), 0);
        assert_eq!(outcome.files_skipped(), 0);
        assert_eq!(outcome.bytes_written, 0);
        assert_eq!(outcome.redundant_edges, 0);
    }

    #[test]
    fn test_default_equals_new() {
        let new_outcome = OutputOutcome::new();
        let default_outcome = OutputOutcome::default();

        assert_eq!(new_outcome.files_written(), default_outcome.files_written());
        assert_eq!(new_outcome.bytes_written, default_outcome.bytes_written);
    }

    // ------------------------------------------------------------------------
    // File Recording Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_record_file() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);

        assert_eq!(outcome.files_written(), 1);
        assert_eq!(outcome.bytes_written, 100);
        assert!(!outcome.is_empty());
    }

    #[test]
    fn test_record_multiple_files() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("a.rs", 100);
        outcome.record_file("b.rs", 200);
        outcome.record_file("c.rs", 300);

        assert_eq!(outcome.files_written(), 3);
        assert_eq!(outcome.bytes_written, 600);
    }

    #[test]
    fn test_record_file_zero_bytes() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("empty.txt", 0);

        assert_eq!(outcome.files_written(), 1);
        assert_eq!(outcome.bytes_written, 0);
    }

    #[test]
    fn test_iter_files() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);
        outcome.record_file("lib.rs", 200);

        let files: Vec<_> = outcome.iter_files().collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "main.rs");
        assert_eq!(files[0].bytes, 100);
        assert_eq!(files[1].path, "lib.rs");
        assert_eq!(files[1].bytes, 200);
    }

    // ------------------------------------------------------------------------
    // Directory Recording Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_record_directory() {
        let mut outcome = OutputOutcome::new();
        outcome.record_directory("src");

        assert_eq!(outcome.directories_created(), 1);
        assert!(!outcome.is_empty());
    }

    #[test]
    fn test_record_directory_deduplication() {
        let mut outcome = OutputOutcome::new();
        outcome.record_directory("src");
        outcome.record_directory("src"); // Duplicate
        outcome.record_directory("tests");

        assert_eq!(outcome.directories_created(), 2);
    }

    #[test]
    fn test_iter_directories() {
        let mut outcome = OutputOutcome::new();
        outcome.record_directory("src");
        outcome.record_directory("tests");

        let dirs: HashSet<_> = outcome.iter_directories().collect();
        assert!(dirs.contains(&"src".to_string()));
        assert!(dirs.contains(&"tests".to_string()));
    }

    // ------------------------------------------------------------------------
    // Skip Recording Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_record_skip() {
        let mut outcome = OutputOutcome::new();
        outcome.record_skip("unchanged.txt");

        assert_eq!(outcome.files_skipped(), 1);
        assert!(!outcome.is_empty());
    }

    #[test]
    fn test_record_multiple_skips() {
        let mut outcome = OutputOutcome::new();
        outcome.record_skip("a.txt");
        outcome.record_skip("b.txt");

        assert_eq!(outcome.files_skipped(), 2);
    }

    #[test]
    fn test_iter_skipped() {
        let mut outcome = OutputOutcome::new();
        outcome.record_skip("a.txt");
        outcome.record_skip("b.txt");

        let skipped: Vec<_> = outcome.iter_skipped().collect();
        assert_eq!(skipped.len(), 2);
    }

    // ------------------------------------------------------------------------
    // Redundant Edges Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_record_redundant_edges() {
        let mut outcome = OutputOutcome::new();
        outcome.record_redundant_edges(5);
        outcome.record_redundant_edges(3);

        assert_eq!(outcome.redundant_edges, 8);
    }

    // ------------------------------------------------------------------------
    // Merge Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_merge_files() {
        let mut outcome1 = OutputOutcome::new();
        outcome1.record_file("a.rs", 100);

        let mut outcome2 = OutputOutcome::new();
        outcome2.record_file("b.rs", 200);

        outcome1.merge(outcome2);

        assert_eq!(outcome1.files_written(), 2);
        assert_eq!(outcome1.bytes_written, 300);
    }

    #[test]
    fn test_merge_directories() {
        let mut outcome1 = OutputOutcome::new();
        outcome1.record_directory("src");

        let mut outcome2 = OutputOutcome::new();
        outcome2.record_directory("tests");

        outcome1.merge(outcome2);

        assert_eq!(outcome1.directories_created(), 2);
    }

    #[test]
    fn test_merge_skipped() {
        let mut outcome1 = OutputOutcome::new();
        outcome1.record_skip("a.txt");

        let mut outcome2 = OutputOutcome::new();
        outcome2.record_skip("b.txt");

        outcome1.merge(outcome2);

        assert_eq!(outcome1.files_skipped(), 2);
    }

    #[test]
    fn test_merge_redundant_edges() {
        let mut outcome1 = OutputOutcome::new();
        outcome1.record_redundant_edges(5);

        let mut outcome2 = OutputOutcome::new();
        outcome2.record_redundant_edges(3);

        outcome1.merge(outcome2);

        assert_eq!(outcome1.redundant_edges, 8);
    }

    #[test]
    fn test_merge_empty() {
        let mut outcome1 = OutputOutcome::new();
        outcome1.record_file("a.rs", 100);

        let outcome2 = OutputOutcome::new();
        outcome1.merge(outcome2);

        assert_eq!(outcome1.files_written(), 1);
    }

    // ------------------------------------------------------------------------
    // Summary and Display Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_summary_empty() {
        let outcome = OutputOutcome::new();
        assert_eq!(outcome.summary(), "No changes");
    }

    #[test]
    fn test_summary_one_file() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);

        let summary = outcome.summary();
        assert!(summary.contains("1 file written"));
    }

    #[test]
    fn test_summary_multiple_files() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("a.rs", 100);
        outcome.record_file("b.rs", 200);

        let summary = outcome.summary();
        assert!(summary.contains("2 files written"));
    }

    #[test]
    fn test_summary_one_directory() {
        let mut outcome = OutputOutcome::new();
        outcome.record_directory("src");

        let summary = outcome.summary();
        assert!(summary.contains("1 directory created"));
    }

    #[test]
    fn test_summary_multiple_directories() {
        let mut outcome = OutputOutcome::new();
        outcome.record_directory("src");
        outcome.record_directory("tests");

        let summary = outcome.summary();
        assert!(summary.contains("2 directories created"));
    }

    #[test]
    fn test_summary_skipped() {
        let mut outcome = OutputOutcome::new();
        outcome.record_skip("unchanged.txt");

        let summary = outcome.summary();
        assert!(summary.contains("1 file skipped"));
    }

    #[test]
    fn test_summary_combined() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);
        outcome.record_directory("src");
        outcome.record_skip("unchanged.txt");

        let summary = outcome.summary();
        assert!(summary.contains("1 file written"));
        assert!(summary.contains("1 directory created"));
        assert!(summary.contains("1 file skipped"));
    }

    #[test]
    fn test_display() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);

        let display = format!("{}", outcome);
        assert!(display.contains("1 file written"));
    }

    // ------------------------------------------------------------------------
    // FileWritten Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_written_new() {
        let fw = FileWritten::new("test.rs", 100);
        assert_eq!(fw.path, "test.rs");
        assert_eq!(fw.bytes, 100);
    }

    #[test]
    fn test_file_written_equality() {
        let fw1 = FileWritten::new("test.rs", 100);
        let fw2 = FileWritten::new("test.rs", 100);
        let fw3 = FileWritten::new("other.rs", 100);

        assert_eq!(fw1, fw2);
        assert_ne!(fw1, fw3);
    }

    #[test]
    fn test_file_written_clone() {
        let fw = FileWritten::new("test.rs", 100);
        let cloned = fw.clone();

        assert_eq!(fw, cloned);
    }

    // ------------------------------------------------------------------------
    // Clone and Debug Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_outcome_clone() {
        let mut outcome = OutputOutcome::new();
        outcome.record_file("main.rs", 100);
        outcome.record_directory("src");

        let cloned = outcome.clone();

        assert_eq!(cloned.files_written(), outcome.files_written());
        assert_eq!(cloned.bytes_written, outcome.bytes_written);
    }

    #[test]
    fn test_outcome_debug() {
        let outcome = OutputOutcome::new();
        let debug_str = format!("{:?}", outcome);

        assert!(debug_str.contains("OutputOutcome"));
    }
}

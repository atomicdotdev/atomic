//! The `add` command for tracking files in the repository.
//!
//! This module implements the `atomic add` command, which adds files and
//! directories to the repository's tracking system. Tracking is the first
//! step in version controlling a file - it establishes a connection between
//! the file in the working copy and the repository's internal structure.
//!
//! # Usage
//!
//! ```text
//! atomic add [OPTIONS] <FILES>...
//!
//! Arguments:
//!   <FILES>...  Files or directories to add
//!
//! Options:
//!   -A, --all           Add all untracked files
//!   -n, --dry-run       Show what would be added without doing it
//!   -f, --force         Force add ignored files
//!   -r, --recursive     Recursively add directory contents (default)
//!   --no-recursive      Don't recursively add directory contents
//!   -h, --help          Print help information
//! ```
//!
//! # Tracking vs Recording
//!
//! It's important to understand the difference between tracking and recording:
//!
//! - **Tracking** (`add`): Marks a file for version control by allocating an
//!   inode and creating tree mappings. This does NOT store the file's content.
//! - **Recording** (`record`): Creates a change from tracked files, storing
//!   their content in the repository graph.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                        File Lifecycle                                │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  Untracked  ──add──▶  Tracked  ──record──▶  Recorded               │
//! │     file              (inode)              (in graph)               │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Examples
//!
//! Add a single file:
//! ```text
//! $ atomic add src/main.rs
//! Adding: src/main.rs
//! ✓ Added 1 file
//! ```
//!
//! Add multiple files:
//! ```text
//! $ atomic add src/main.rs src/lib.rs README.md
//! Adding: src/main.rs
//! Adding: src/lib.rs
//! Adding: README.md
//! ✓ Added 3 files
//! ```
//!
//! Add a directory recursively:
//! ```text
//! $ atomic add src/
//! Adding: src/main.rs
//! Adding: src/lib.rs
//! Adding: src/utils/mod.rs
//! Adding: src/utils/helpers.rs
//! ✓ Added 4 files, 2 directories
//! ```
//!
//! Dry run to see what would be added:
//! ```text
//! $ atomic add --dry-run src/
//! Would add: src/main.rs
//! Would add: src/lib.rs
//! (dry run - no changes made)
//! ```
//!
//! Add all untracked files:
//! ```text
//! $ atomic add --all
//! Adding: new_file.rs
//! Adding: another_file.txt
//! ✓ Added 2 files
//! ```

use std::path::PathBuf;

use clap::Parser;

use atomic_repository::status::StatusOptions;
use atomic_repository::tracking::{TrackingOptions, TrackingStats};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    added, hint, info, print_blank, print_hint, print_success, print_warning, warning,
};

// Add Command

/// Add files to be tracked by the repository.
///
/// The `add` command marks files and directories for version control tracking.
/// This is the first step in bringing a file under Atomic's management.
///
/// # Behavior
///
/// - Files: Added directly to tracking
/// - Directories: Added recursively by default (use `--no-recursive` to change)
/// - Already tracked files: Silently skipped unless `--force` is used
/// - Ignored files: Skipped unless `--force` is used
///
/// # Options
///
/// - `--all` / `-A`: Add all untracked files in the repository
/// - `--dry-run` / `-n`: Preview what would be added without making changes
/// - `--force` / `-f`: Force add ignored or already tracked files
/// - `--recursive` / `-r`: Recursively add directory contents (default: true)
/// - `--no-recursive`: Don't recursively add directory contents
#[derive(Parser, Debug)]
pub struct Add {
    /// Files or directories to add to tracking.
    ///
    /// Paths can be relative or absolute. Directories will be added
    /// recursively by default.
    #[arg(value_name = "FILES", required_unless_present = "all")]
    pub files: Vec<String>,

    /// Add all untracked files in the repository.
    ///
    /// This is equivalent to running `atomic status` to find untracked files
    /// and then adding them all. Use with caution in large repositories.
    #[arg(short = 'A', long = "all", conflicts_with = "files")]
    pub all: bool,

    /// Dry run - show what would be added without doing it.
    ///
    /// When enabled, displays the files that would be added but doesn't
    /// actually modify the repository. Useful for previewing changes.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force add ignored files.
    ///
    /// By default, files matching ignore patterns are skipped. Use this
    /// flag to add them anyway.
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Recursively add directory contents.
    ///
    /// When adding a directory, also add all files within it.
    /// This is the default behavior.
    #[arg(short = 'r', long = "recursive", default_value = "true")]
    pub recursive: bool,

    /// Don't recursively add directory contents.
    ///
    /// When adding a directory, only add the directory itself,
    /// not its contents.
    #[arg(long = "no-recursive", conflicts_with = "recursive")]
    pub no_recursive: bool,

    /// Track empty directories explicitly.
    ///
    /// By default, Atomic only tracks files (directories are created implicitly
    /// when needed). Use this flag to explicitly track empty directories.
    ///
    /// Unlike Git (which requires `.keep` files), Atomic supports tracking
    /// empty directories as first-class citizens in the repository graph.
    ///
    /// # Example
    ///
    /// ```text
    /// $ atomic add --directory src/empty_module/
    /// Adding directory: src/empty_module/
    /// ✓ Added 1 directory
    /// ```
    #[arg(short = 'd', long = "directory")]
    pub directory: bool,
}

impl Add {
    /// Create a new Add command with default settings.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let add = Add::new();
    /// ```
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            all: false,
            dry_run: false,
            force: false,
            recursive: true,
            no_recursive: false,
            directory: false,
        }
    }

    /// Convert command options to TrackingOptions.
    fn get_tracking_options(&self) -> TrackingOptions {
        let recursive = self.recursive && !self.no_recursive;

        TrackingOptions::default()
            .with_recursive(recursive)
            .with_force(self.force)
    }

    /// Collect all untracked files in the repository.
    fn collect_untracked_files(&self, repo: &Repository) -> CliResult<Vec<PathBuf>> {
        let status = repo
            .status(StatusOptions::default())
            .map_err(|e| CliError::Internal(e.into()))?;

        Ok(status.untracked().map(|e| e.path().to_path_buf()).collect())
    }

    /// Add a single path to tracking.
    fn add_path(
        &self,
        repo: &Repository,
        path: &str,
        options: &TrackingOptions,
    ) -> CliResult<TrackingStats> {
        // Convert to PathBuf for the repository API
        let path_buf = PathBuf::from(path);

        // Check if the path exists relative to repo root
        let full_path = repo.root().join(&path_buf);
        if !full_path.exists() {
            return Err(CliError::FileNotFound { path: path_buf });
        }

        // Check if path is inside .atomic
        if repo.is_internal_path(&path_buf) {
            return Err(CliError::PathOutsideRepository { path: path_buf });
        }

        // Handle explicit directory tracking
        if self.directory {
            // Verify it's actually a directory
            if !full_path.is_dir() {
                return Err(CliError::InvalidArgument {
                    message: format!("--directory: path '{}' is not a directory", path),
                });
            }

            // Use the dedicated directory tracking method
            return repo
                .add_directory(&path_buf, options.clone())
                .map_err(|e| match e {
                    atomic_repository::RepositoryError::PathOutsideRepository { path } => {
                        CliError::PathOutsideRepository { path }
                    }
                    atomic_repository::RepositoryError::FileAlreadyTracked { path } => {
                        CliError::FileAlreadyTracked { path }
                    }
                    atomic_repository::RepositoryError::PathIgnored { path } => {
                        CliError::PathIgnored { path }
                    }
                    other => CliError::Internal(other.into()),
                });
        }

        // Perform the standard add operation
        repo.add(&path_buf, options.clone()).map_err(|e| match e {
            atomic_repository::RepositoryError::PathOutsideRepository { path } => {
                CliError::PathOutsideRepository { path }
            }
            atomic_repository::RepositoryError::FileAlreadyTracked { path } => {
                CliError::FileAlreadyTracked { path }
            }
            atomic_repository::RepositoryError::PathIgnored { path } => {
                CliError::PathIgnored { path }
            }
            other => CliError::Internal(other.into()),
        })
    }

    /// Print the results of adding files.
    fn print_results(&self, total_stats: &AggregateStats) {
        if self.dry_run {
            print_blank();
            println!("{}", hint("(dry run - no changes made)"));
            return;
        }

        if total_stats.total_added() == 0 {
            if total_stats.skipped > 0 {
                print_warning(&format!(
                    "No files added ({} already tracked)",
                    total_stats.skipped
                ));
            } else {
                println!("{}", info("Nothing to add"));
            }
            return;
        }

        // Format the success message
        let mut parts = Vec::new();

        if total_stats.files_added > 0 {
            parts.push(format_count(total_stats.files_added, "file", "files"));
        }

        if total_stats.directories_added > 0 {
            parts.push(format_count(
                total_stats.directories_added,
                "directory",
                "directories",
            ));
        }

        if total_stats.explicit_directories > 0 {
            parts.push(format_count(
                total_stats.explicit_directories,
                "empty directory",
                "empty directories",
            ));
        }

        let message = format!("Added {}", parts.join(", "));
        print_success(&message);

        // Show skipped files hint if any
        if total_stats.skipped > 0 {
            print_hint(&format!(
                "{} path(s) already tracked (skipped)",
                total_stats.skipped
            ));
        }
    }
}

impl Default for Add {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Add {
    /// Execute the add command.
    ///
    /// This method:
    /// 1. Finds and opens the repository
    /// 2. Collects files to add (from arguments or --all)
    /// 3. Adds each file to tracking
    /// 4. Prints the results
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found
    /// - A file doesn't exist
    /// - A file is inside .atomic/
    /// - A database error occurs
    fn run(&self) -> CliResult<()> {
        // Find the repository root
        let repo_root = find_repository_root()?;

        // Open the repository
        let repo = Repository::open(&repo_root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;

        // Get tracking options
        let mut options = self.get_tracking_options();
        if self.dry_run {
            options.dry_run = true;
        }

        // Collect files to add
        let files_to_add: Vec<PathBuf> = if self.all {
            self.collect_untracked_files(&repo)?
        } else {
            self.files.iter().map(PathBuf::from).collect()
        };

        if files_to_add.is_empty() {
            if self.all {
                println!("{}", info("No untracked files to add"));
            } else {
                return Err(CliError::InvalidArgument {
                    message: "No files specified. Use 'atomic add <files>' or 'atomic add --all'"
                        .to_string(),
                });
            }
            return Ok(());
        }

        // Add each file and collect stats
        let mut total_stats = AggregateStats::new();
        let mut had_errors = false;

        for path in &files_to_add {
            let path_str = path.to_string_lossy();

            match self.add_path(&repo, &path_str, &options) {
                Ok(stats) => {
                    // Print progress for each file
                    if self.dry_run {
                        if stats.files_added > 0 || stats.directories_added > 0 {
                            println!("Would add: {}", path_str);
                        }
                    } else if stats.files_added > 0 || stats.directories_added > 0 {
                        println!("{}: {}", added("Adding"), path_str);
                    }

                    total_stats.merge(&stats);
                }
                Err(CliError::FileAlreadyTracked { path }) => {
                    // Not an error, just skip
                    total_stats.skipped += 1;
                    if !self.force {
                        println!(
                            "{}: {} (already tracked)",
                            warning("Skipping"),
                            path.display()
                        );
                    }
                }
                Err(CliError::PathIgnored { path }) => {
                    // Not an error, just skip - path is in .atomicignore
                    total_stats.skipped += 1;
                    if !self.force {
                        println!("{}: {} (ignored)", warning("Skipping"), path.display());
                    }
                }
                Err(e) => {
                    eprintln!("{}: {} - {}", warning("Error"), path_str, e);
                    had_errors = true;
                }
            }
        }

        // Print summary
        self.print_results(&total_stats);

        // Return error if any files failed
        if had_errors {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Some files could not be added"
            )));
        }

        // Print next steps hint
        if !self.dry_run && total_stats.total_added() > 0 {
            print_blank();
            print_hint("Use 'atomic record -m \"...\"' to record your changes");
        }

        Ok(())
    }
}

// Helper Types

/// Aggregate statistics from multiple add operations.
#[derive(Debug, Clone, Default)]
struct AggregateStats {
    /// Total files added.
    files_added: usize,
    /// Total directories added.
    directories_added: usize,
    /// Total explicit (empty) directories added.
    explicit_directories: usize,
    /// Total paths skipped.
    skipped: usize,
}

impl AggregateStats {
    /// Create empty aggregate stats.
    fn new() -> Self {
        Self::default()
    }

    /// Merge stats from a single operation.
    fn merge(&mut self, stats: &TrackingStats) {
        self.files_added += stats.files_added;
        self.directories_added += stats.directories_added;
        self.explicit_directories += stats.explicit_directories_added;
        self.skipped += stats.skipped;
    }

    /// Total items added.
    fn total_added(&self) -> usize {
        self.files_added + self.directories_added + self.explicit_directories
    }
}

// Helper Functions

/// Format a count with singular/plural noun.
fn format_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}", count, plural)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    // Add Command Construction Tests

    #[test]
    fn test_add_new() {
        let add = Add::new();
        assert!(add.files.is_empty());
        assert!(!add.all);
        assert!(!add.dry_run);
        assert!(!add.force);
        assert!(add.recursive);
        assert!(!add.no_recursive);
    }

    #[test]
    fn test_add_default() {
        let add = Add::default();
        assert!(add.files.is_empty());
        assert!(!add.all);
        assert!(!add.dry_run);
        assert!(!add.force);
        assert!(add.recursive);
    }

    #[test]
    fn test_add_with_files_vec() {
        let add = Add::new().with_files(vec!["src/main.rs", "src/lib.rs"]);
        assert_eq!(add.files.len(), 2);
        assert_eq!(add.files[0], "src/main.rs");
        assert_eq!(add.files[1], "src/lib.rs");
    }

    #[test]
    fn test_add_with_files_strings() {
        let add = Add::new().with_files(vec![String::from("README.md")]);
        assert_eq!(add.files.len(), 1);
        assert_eq!(add.files[0], "README.md");
    }

    #[test]
    fn test_add_with_all() {
        let add = Add::new().with_all(true);
        assert!(add.all);
    }

    #[test]
    fn test_add_with_dry_run() {
        let add = Add::new().with_dry_run(true);
        assert!(add.dry_run);
    }

    #[test]
    fn test_add_with_force() {
        let add = Add::new().with_force(true);
        assert!(add.force);
    }

    #[test]
    fn test_add_with_recursive_true() {
        let add = Add::new().with_recursive(true);
        assert!(add.recursive);
        assert!(!add.no_recursive);
    }

    #[test]
    fn test_add_with_recursive_false() {
        let add = Add::new().with_recursive(false);
        assert!(!add.recursive);
        assert!(add.no_recursive);
    }

    #[test]
    fn test_add_builder_chain() {
        let add = Add::new()
            .with_files(vec!["src/", "tests/"])
            .with_dry_run(true)
            .with_force(true)
            .with_recursive(true);

        assert_eq!(add.files.len(), 2);
        assert!(add.dry_run);
        assert!(add.force);
        assert!(add.recursive);
    }

    // TrackingOptions Conversion Tests

    #[test]
    fn test_get_tracking_options_default() {
        let add = Add::new();
        let options = add.get_tracking_options();
        assert!(options.recursive);
        assert!(!options.force);
        assert!(!options.dry_run);
    }

    #[test]
    fn test_get_tracking_options_non_recursive() {
        let add = Add::new().with_recursive(false);
        let options = add.get_tracking_options();
        assert!(!options.recursive);
    }

    #[test]
    fn test_get_tracking_options_with_force() {
        let add = Add::new().with_force(true);
        let options = add.get_tracking_options();
        assert!(options.force);
    }

    #[test]
    fn test_get_tracking_options_no_recursive_flag() {
        let mut add = Add::new();
        add.no_recursive = true;
        let options = add.get_tracking_options();
        assert!(!options.recursive);
    }

    // AggregateStats Tests

    #[test]
    fn test_aggregate_stats_new() {
        let stats = AggregateStats::new();
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.directories_added, 0);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn test_aggregate_stats_merge() {
        let mut aggregate = AggregateStats::new();

        let mut stats1 = TrackingStats::new();
        stats1.files_added = 3;
        stats1.directories_added = 1;

        let mut stats2 = TrackingStats::new();
        stats2.files_added = 2;
        stats2.skipped = 1;

        aggregate.merge(&stats1);
        aggregate.merge(&stats2);

        assert_eq!(aggregate.files_added, 5);
        assert_eq!(aggregate.directories_added, 1);
        assert_eq!(aggregate.skipped, 1);
    }

    #[test]
    fn test_aggregate_stats_total_added() {
        let mut stats = AggregateStats::new();
        stats.files_added = 5;
        stats.directories_added = 3;

        assert_eq!(stats.total_added(), 8);
    }

    #[test]
    fn test_aggregate_stats_total_added_empty() {
        let stats = AggregateStats::new();
        assert_eq!(stats.total_added(), 0);
    }

    // Format Count Tests

    #[test]
    fn test_format_count_singular() {
        assert_eq!(format_count(1, "file", "files"), "1 file");
    }

    #[test]
    fn test_format_count_plural() {
        assert_eq!(format_count(5, "file", "files"), "5 files");
    }

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "file", "files"), "0 files");
    }

    #[test]
    fn test_format_count_directory() {
        assert_eq!(format_count(1, "directory", "directories"), "1 directory");
        assert_eq!(format_count(3, "directory", "directories"), "3 directories");
    }

    // Integration Tests (require temp directories)

    /// Guard that restores the current directory when dropped.
    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    #[test]
    #[serial]
    fn test_add_run_outside_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let add = Add::new().with_files(vec!["file.txt"]);
        let result = add.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_add_run_file_not_found() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["nonexistent.txt"]);
        let result = add.run();

        // Should fail because file doesn't exist
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_add_run_single_file() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file to add
        std::fs::write(repo_path.join("test.txt"), "Hello, World!").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["test.txt"]);
        let result = add.run();

        assert!(result.is_ok(), "Add failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_multiple_files() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create files to add
        std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
        std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["file1.txt", "file2.txt"]);
        let result = add.run();

        assert!(result.is_ok(), "Add failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_directory() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create directory with files
        let src_dir = repo_path.join("src");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(src_dir.join("lib.rs"), "// lib").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["src"]);
        let result = add.run();

        assert!(result.is_ok(), "Add failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_dry_run() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file
        std::fs::write(repo_path.join("test.txt"), "Hello").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["test.txt"]).with_dry_run(true);
        let result = add.run();

        assert!(result.is_ok(), "Dry run failed: {:?}", result.err());

        // Verify file was not actually tracked
        {
            let repo = Repository::open(repo_path).unwrap();
            assert!(!repo.is_tracked("test.txt").unwrap_or(true));
        }
    }

    #[test]
    #[serial]
    fn test_add_run_already_tracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let repo = Repository::init(repo_path).unwrap();

            // Create and add a file
            std::fs::write(repo_path.join("test.txt"), "Hello").unwrap();
            repo.add("test.txt", TrackingOptions::default()).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Try to add again
        let add = Add::new().with_files(vec!["test.txt"]);
        let result = add.run();

        // Should succeed (skips already tracked files)
        assert!(result.is_ok(), "Add failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_with_force() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file
        std::fs::write(repo_path.join("test.txt"), "Hello").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["test.txt"]).with_force(true);
        let result = add.run();

        assert!(result.is_ok(), "Add with force failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_non_recursive() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create directory with files
        let src_dir = repo_path.join("src");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["src"]).with_recursive(false);
        let result = add.run();

        assert!(
            result.is_ok(),
            "Add non-recursive failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_add_run_all_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create untracked files
        std::fs::write(repo_path.join("file1.txt"), "Content 1").unwrap();
        std::fs::write(repo_path.join("file2.txt"), "Content 2").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_all(true);
        let result = add.run();

        assert!(result.is_ok(), "Add --all failed: {:?}", result.err());
    }

    #[test]
    #[serial]
    fn test_add_run_all_no_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository (no untracked files)
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_all(true);
        let result = add.run();

        // Should succeed with a message about nothing to add
        assert!(
            result.is_ok(),
            "Add --all with no files failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_add_run_nested_directory() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create nested directory structure
        let nested = repo_path.join("src").join("utils").join("helpers");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("mod.rs"), "// mod").unwrap();
        std::fs::write(repo_path.join("src").join("main.rs"), "fn main() {}").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["src"]);
        let result = add.run();

        assert!(
            result.is_ok(),
            "Add nested directory failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_add_cannot_add_atomic_dir() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec![".atomic"]);
        let result = add.run();

        // Should succeed but skip the .atomic directory (it's ignored)
        // The command gracefully skips ignored paths rather than erroring
        assert!(result.is_ok());
    }

    // Edge Case Tests

    #[test]
    fn test_add_empty_files_list() {
        let add = Add::new();
        assert!(add.files.is_empty());
    }

    #[test]
    fn test_add_with_files_iterator() {
        let files = ["a.rs", "b.rs", "c.rs"];
        let add = Add::new().with_files(files.iter().copied());
        assert_eq!(add.files.len(), 3);
    }

    #[test]
    fn test_add_recursive_and_no_recursive_conflict() {
        // When no_recursive is set, recursive should be effectively false
        let mut add = Add::new();
        add.recursive = true;
        add.no_recursive = true;

        let options = add.get_tracking_options();
        assert!(!options.recursive);
    }

    #[test]
    fn test_aggregate_stats_default() {
        let stats = AggregateStats::default();
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.directories_added, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.total_added(), 0);
    }

    #[test]
    fn test_aggregate_stats_merge_multiple() {
        let mut aggregate = AggregateStats::new();

        for i in 0..5 {
            let mut stats = TrackingStats::new();
            stats.files_added = i;
            aggregate.merge(&stats);
        }

        // 0 + 1 + 2 + 3 + 4 = 10
        assert_eq!(aggregate.files_added, 10);
    }

    #[test]
    fn test_format_count_large_numbers() {
        assert_eq!(format_count(1000, "item", "items"), "1000 items");
        assert_eq!(format_count(999999, "thing", "things"), "999999 things");
    }

    #[test]
    #[serial]
    fn test_add_with_path_containing_spaces() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file with spaces in name
        std::fs::write(repo_path.join("my file.txt"), "content").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["my file.txt"]);
        let result = add.run();

        assert!(
            result.is_ok(),
            "Add file with spaces failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_add_with_unicode_filename() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file with unicode name
        std::fs::write(repo_path.join("文件.txt"), "内容").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        let add = Add::new().with_files(vec!["文件.txt"]);
        let result = add.run();

        assert!(
            result.is_ok(),
            "Add unicode file failed: {:?}",
            result.err()
        );
    }

    #[test]
    #[serial]
    fn test_add_verify_tracking_after_add() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file
        std::fs::write(repo_path.join("tracked.txt"), "content").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        // Add the file
        let add = Add::new().with_files(vec!["tracked.txt"]);
        add.run().unwrap();

        // Verify it's tracked
        {
            let repo = Repository::open(repo_path).unwrap();
            assert!(repo.is_tracked("tracked.txt").unwrap());
        }
    }

    #[test]
    #[serial]
    fn test_add_dry_run_does_not_track() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Create a file
        std::fs::write(repo_path.join("not_tracked.txt"), "content").unwrap();

        std::env::set_current_dir(repo_path).unwrap();

        // Dry run add
        let add = Add::new()
            .with_files(vec!["not_tracked.txt"])
            .with_dry_run(true);
        add.run().unwrap();

        // Verify it's NOT tracked
        {
            let repo = Repository::open(repo_path).unwrap();
            assert!(!repo.is_tracked("not_tracked.txt").unwrap());
        }
    }
}

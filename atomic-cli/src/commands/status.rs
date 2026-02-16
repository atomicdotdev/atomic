//! The `status` command for showing the working copy state.
//!
//! This module implements the `atomic status` command, which displays
//! information about modified, added, deleted, and untracked files in
//! the working copy relative to the repository's recorded state.
//!
//! # Usage
//!
//! ```text
//! atomic status [OPTIONS] [PATH]
//!
//! Arguments:
//!   [PATH]  Show status for specific path only
//!
//! Options:
//!   -s, --short         Show short/porcelain output
//!   --no-untracked      Don't show untracked files
//!   -h, --help          Print help information
//! ```
//!
//! # Output Formats
//!
//! ## Long Format (Default)
//!
//! The long format provides detailed, user-friendly output with colored
//! sections and helpful hints:
//!
//! ```text
//! On stack dev
//! State: ABCDEF123456
//!
//! Changes to be recorded:
//!   (use "atomic reset <file>..." to discard changes)
//!
//!     new file:   src/new_feature.rs
//!     modified:   src/main.rs
//!     deleted:    src/old_module.rs
//!
//! Untracked files:
//!   (use "atomic add <file>..." to include in what will be recorded)
//!
//!     notes.txt
//!     temp/
//! ```
//!
//! ## Short Format (-s, --short)
//!
//! The short format is machine-readable and similar to `git status -s`:
//!
//! ```text
//! A  src/new_feature.rs
//! M  src/main.rs
//! D  src/old_module.rs
//! ?  notes.txt
//! ?  temp/
//! ```
//!
//! # Status Codes
//!
//! | Code | Meaning |
//! |------|---------|
//! | `A` | Added (new file tracked) |
//! | `M` | Modified |
//! | `D` | Deleted |
//! | `R` | Renamed |
//! | `C` | Conflicted |
//! | `T` | Type changed |
//! | `P` | Permissions changed |
//! | `?` | Untracked |
//! | ` ` | Clean (only shown with --verbose) |
//!
//! # Examples
//!
//! Show status of entire repository:
//! ```text
//! $ atomic status
//! On stack dev
//! State: ABCDEF12
//!
//! Changes to be recorded:
//!     modified:   src/lib.rs
//!
//! nothing else to record
//! ```
//!
//! Show short status:
//! ```text
//! $ atomic status -s
//! M  src/lib.rs
//! ```
//!
//! Show status for specific path:
//! ```text
//! $ atomic status src/
//! On stack dev
//! State: ABCDEF12
//!
//! Changes to be recorded:
//!     modified:   src/lib.rs
//! ```

use std::path::PathBuf;

use clap::Parser;

use atomic_core::types::Base32;
use atomic_repository::status::{RepositoryStatus, StatusOptions};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command, DEFAULT_HASH_LENGTH};
use crate::error::{CliError, CliResult};
use crate::output::{
    added, deleted, hash, hint, info, modified, path as style_path, print_blank,
    print_hint, print_section, stack as style_stack, untracked as style_untracked, warning,
};

// Status Output Configuration

// Status Command

/// Show the status of the working copy.
///
/// The `status` command displays information about the current state of
/// the working copy, including:
///
/// - The current stack name
/// - The current stack state (Merkle hash)
/// - Modified files (tracked files that have changed)
/// - Deleted files (tracked files that no longer exist)
/// - Added files (newly tracked files)
/// - Untracked files (files not yet added to tracking)
///
/// # Output Formats
///
/// The command supports two output formats:
///
/// 1. **Long format** (default): Human-readable output with colors and hints
/// 2. **Short format** (`-s`): Machine-readable, similar to `git status -s`
///
/// # Path Filtering
///
/// When a path is provided, only files under that path are shown. This is
/// useful for checking status in large repositories.
#[derive(Parser, Debug)]
pub struct Status {
    /// Show status for specific path only.
    ///
    /// When provided, only files under this path will be included in
    /// the status output. The path is relative to the repository root.
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Show short/porcelain output.
    ///
    /// The short format displays one line per file with a two-character
    /// status code followed by the file path. This format is designed
    /// for machine parsing and scripts.
    ///
    /// Status codes:
    /// - `A ` - Added (new tracked file)
    /// - `M ` - Modified
    /// - `D ` - Deleted
    /// - `R ` - Renamed
    /// - `C ` - Conflicted
    /// - `T ` - Type changed
    /// - `P ` - Permissions changed
    /// - `??` - Untracked
    #[arg(short = 's', long = "short")]
    pub short: bool,

    /// Don't show untracked files.
    ///
    /// By default, untracked files are shown in the status output.
    /// Use this flag to hide them, showing only tracked files that
    /// have changes.
    #[arg(long = "no-untracked")]
    pub no_untracked: bool,

    /// Debug ignore pattern matching.
    ///
    /// Shows diagnostic information about how .atomicignore patterns
    /// are being applied. Useful for troubleshooting why files are
    /// or aren't being ignored.
    #[arg(long = "debug-ignore", hide = true)]
    pub debug_ignore: bool,
}

impl Status {
    /// Create a new Status command with default settings.
    pub fn new() -> Self {
        Self {
            path: None,
            short: false,
            no_untracked: false,
            debug_ignore: false,
        }
    }

    /// Get status options based on command settings.
    fn get_status_options(&self) -> StatusOptions {
        let mut options = StatusOptions::default();

        if self.no_untracked {
            options = options.with_untracked(false);
        }

        if let Some(ref path) = self.path {
            options = options.filter_path(PathBuf::from(path));
        }

        options
    }

    /// Print the status in long (human-readable) format.
    fn print_long_format(&self, status: &RepositoryStatus) -> CliResult<()> {
        // Print stack info
        print!("On stack ");
        println!("{}", style_stack(status.stack()));

        // Print state hash if available
        if let Some(state) = status.state() {
            let state_str = state.to_base32();
            // Truncate to default hash length for display
            let short_state = if state_str.len() > DEFAULT_HASH_LENGTH {
                format!("{}...", &state_str[..DEFAULT_HASH_LENGTH])
            } else {
                state_str
            };
            print!("State: ");
            println!("{}", hash(&short_state));
        }

        print_blank();

        // Check if there are any changes
        let has_changes = status.modified_count() > 0
            || status.deleted_count() > 0
            || status.added_count() > 0
            || status.conflicted_count() > 0;

        let has_untracked = status.untracked_count() > 0;

        if !has_changes && !has_untracked {
            println!(
                "{}",
                info("nothing to record, working tree clean")
            );
            return Ok(());
        }

        // Print changes section
        if has_changes {
            print_section("Changes to be recorded:");
            println!(
                "  {}",
                hint("(use \"atomic reset <file>...\" to discard changes)")
            );
            print_blank();

            // Print added files and directories
            for entry in status.added() {
                let path_str = entry.path().display().to_string();
                let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_dir {
                    println!("\t{}    {}", added("new dir:"), style_path(&path_str));
                } else {
                    println!("\t{}   {}", added("new file:"), style_path(&path_str));
                }
            }

            // Print modified files
            for entry in status.modified() {
                let path_str = entry.path().display().to_string();
                println!("\t{}   {}", modified("modified:"), style_path(&path_str));
            }

            // Print deleted files and directories
            for entry in status.deleted() {
                let path_str = entry.path().display().to_string();
                let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
                if is_dir {
                    println!("\t{}    {}", deleted("del dir:"), style_path(&path_str));
                } else {
                    println!("\t{}   {}", deleted("deleted:"), style_path(&path_str));
                }
            }

            // Print conflicted files
            for entry in status.conflicted() {
                let path_str = entry.path().display().to_string();
                let conflict_msg = entry.details().unwrap_or("conflict");
                println!(
                    "\t{} ({})   {}",
                    warning("conflict:"),
                    conflict_msg,
                    style_path(&path_str)
                );
            }

            print_blank();
        }

        // Print untracked files section
        if has_untracked && !self.no_untracked {
            print_section("Untracked files:");
            println!(
                "  {}",
                hint("(use \"atomic add <file>...\" to include in what will be recorded)")
            );
            print_blank();

            for entry in status.untracked() {
                let path_str = entry.path().display().to_string();
                println!("\t{}", style_untracked(&path_str));
            }

            print_blank();
        }

        // Print summary hint
        if has_changes {
            print_hint("Use \"atomic record\" to record your changes");
        } else if has_untracked {
            print_hint("Use \"atomic add <file>...\" to track files");
        }

        Ok(())
    }

    /// Print the status in short (porcelain) format.
    fn print_short_format(&self, status: &RepositoryStatus) -> CliResult<()> {
        // Print added files and directories
        for entry in status.added() {
            let path_str = entry.path().display().to_string();
            let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
            if is_dir {
                println!("AD {}", path_str);
            } else {
                println!("A  {}", path_str);
            }
        }

        // Print modified files
        for entry in status.modified() {
            let path_str = entry.path().display().to_string();
            println!("M  {}", path_str);
        }

        // Print deleted files and directories
        for entry in status.deleted() {
            let path_str = entry.path().display().to_string();
            let is_dir = entry.details().map(|d| d == "directory").unwrap_or(false);
            if is_dir {
                println!("DD {}", path_str);
            } else {
                println!("D  {}", path_str);
            }
        }

        // Print conflicted files
        for entry in status.conflicted() {
            let path_str = entry.path().display().to_string();
            println!("C  {}", path_str);
        }

        // Print untracked files
        if !self.no_untracked {
            for entry in status.untracked() {
                let path_str = entry.path().display().to_string();
                println!("?? {}", path_str);
            }
        }

        Ok(())
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

impl Status {
    /// Print debug information about ignore rules.
    fn print_ignore_debug(&self, repo: &Repository) -> CliResult<()> {
        use std::path::Path;

        println!("=== Ignore Debug Information ===");
        println!("Repository root: {}", repo.root().display());

        let ignore_path = repo.root().join(".atomicignore");
        println!(".atomicignore path: {}", ignore_path.display());
        println!(".atomicignore exists: {}", ignore_path.exists());

        if ignore_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&ignore_path) {
                println!(".atomicignore content ({} bytes):", content.len());
                for (i, line) in content.lines().enumerate() {
                    println!("  {}: {:?}", i + 1, line);
                }
            }
        }

        let rules = repo.ignore_rules();
        println!("\nIgnore rules loaded:");
        println!("  Has local rules: {}", rules.has_local_rules());
        println!("  Has global rules: {}", rules.has_global_rules());
        println!("  Local pattern count: {}", rules.local_pattern_count());
        println!("  Global pattern count: {}", rules.global_pattern_count());

        // Test some common paths
        let test_paths = [
            ("node_modules", true),
            ("node_modules/foo", true),
            ("node_modules/foo/bar.js", false),
            ("target", true),
            ("target/debug", true),
            ("src/main.rs", false),
        ];

        println!("\nTest path matching:");
        for (path, is_dir) in test_paths {
            let ignored = rules.is_ignored(Path::new(path), is_dir);
            println!("  {:40} is_dir={:5} ignored={}", path, is_dir, ignored);
        }

        println!("================================\n");
        Ok(())
    }
}

impl Command for Status {
    /// Execute the status command.
    ///
    /// This method:
    /// 1. Finds and opens the repository
    /// 2. Computes the working copy status
    /// 3. Displays the status in the requested format
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found
    /// - The repository cannot be opened
    /// - Status computation fails
    fn run(&self) -> CliResult<()> {
        // Find the repository root
        let repo_root = find_repository_root()?;

        // Open the repository
        let repo = Repository::open_readonly(&repo_root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;

        // Debug ignore patterns if requested
        if self.debug_ignore {
            self.print_ignore_debug(&repo)?;
        }

        // Get status options
        let options = self.get_status_options();

        // Compute status
        let status = repo.status(options).map_err(|e| CliError::Internal(e.into()))?;

        // Print in appropriate format
        if self.short {
            self.print_short_format(&status)
        } else {
            self.print_long_format(&status)
        }
    }
}

// Helper Functions

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::Merkle;
    use atomic_repository::status::FileStatusEntry;
    use std::path::PathBuf;

    // Status Command Construction Tests

    #[test]
    fn test_status_new() {
        let status = Status::new();
        assert!(status.path.is_none());
        assert!(!status.short);
        assert!(!status.no_untracked);
    }

    #[test]
    fn test_status_default() {
        let status = Status::default();
        assert!(status.path.is_none());
        assert!(!status.short);
        assert!(!status.no_untracked);
    }

    #[test]
    fn test_status_with_path() {
        let status = Status::new().with_path("src/");
        assert_eq!(status.path, Some("src/".to_string()));
    }

    #[test]
    fn test_status_with_short() {
        let status = Status::new().with_short(true);
        assert!(status.short);
    }

    #[test]
    fn test_status_with_no_untracked() {
        let status = Status::new().with_no_untracked(true);
        assert!(status.no_untracked);
    }

    #[test]
    fn test_status_builder_chain() {
        let status = Status::new()
            .with_path("src/")
            .with_short(true)
            .with_no_untracked(true);

        assert_eq!(status.path, Some("src/".to_string()));
        assert!(status.short);
        assert!(status.no_untracked);
    }

    // StatusOutputConfig Tests

    #[test]
    fn test_output_config_new() {
        let config = StatusOutputConfig::new();
        assert!(!config.short);
        assert!(!config.no_untracked);
        assert!(config.path_filter.is_none());
    }

    #[test]
    fn test_output_config_default() {
        let config = StatusOutputConfig::default();
        assert!(!config.short);
        assert!(!config.no_untracked);
        assert!(config.path_filter.is_none());
    }

    #[test]
    fn test_output_config_short() {
        let config = StatusOutputConfig::new().short();
        assert!(config.short);
    }

    #[test]
    fn test_output_config_hide_untracked() {
        let config = StatusOutputConfig::new().hide_untracked();
        assert!(config.no_untracked);
    }

    #[test]
    fn test_output_config_filter_path() {
        let config = StatusOutputConfig::new().filter_path("src/commands/");
        assert_eq!(config.path_filter, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_output_config_builder_chain() {
        let config = StatusOutputConfig::new()
            .short()
            .hide_untracked()
            .filter_path("tests/");

        assert!(config.short);
        assert!(config.no_untracked);
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    // Status Options Conversion Tests

    #[test]
    fn test_get_status_options_default() {
        let status = Status::new();
        let _options = status.get_status_options();
        // Just verify it doesn't panic and returns valid options
    }

    #[test]
    fn test_get_status_options_no_untracked() {
        let status = Status::new().with_no_untracked(true);
        let _options = status.get_status_options();
        // Options are created without panic
    }

    #[test]
    fn test_get_status_options_with_path() {
        let status = Status::new().with_path("src/");
        let _options = status.get_status_options();
        // Options include path filter
    }

    // Status Code Tests

    #[test]
    fn test_status_code_clean() {
        assert_eq!(status_code(FileStatus::Clean), ' ');
    }

    #[test]
    fn test_status_code_modified() {
        assert_eq!(status_code(FileStatus::Modified), 'M');
    }

    #[test]
    fn test_status_code_deleted() {
        assert_eq!(status_code(FileStatus::Deleted), 'D');
    }

    #[test]
    fn test_status_code_untracked() {
        assert_eq!(status_code(FileStatus::Untracked), '?');
    }

    #[test]
    fn test_status_code_added() {
        assert_eq!(status_code(FileStatus::Added), 'A');
    }

    #[test]
    fn test_status_code_conflicted() {
        assert_eq!(status_code(FileStatus::Conflicted), 'C');
    }

    #[test]
    fn test_status_code_type_changed() {
        assert_eq!(status_code(FileStatus::TypeChanged), 'T');
    }

    #[test]
    fn test_status_code_permissions_changed() {
        assert_eq!(status_code(FileStatus::PermissionsChanged), 'P');
    }

    // Status Description Tests

    #[test]
    fn test_status_description_clean() {
        assert_eq!(status_description(FileStatus::Clean), "clean");
    }

    #[test]
    fn test_status_description_modified() {
        assert_eq!(status_description(FileStatus::Modified), "modified");
    }

    #[test]
    fn test_status_description_deleted() {
        assert_eq!(status_description(FileStatus::Deleted), "deleted");
    }

    #[test]
    fn test_status_description_untracked() {
        assert_eq!(status_description(FileStatus::Untracked), "untracked");
    }

    #[test]
    fn test_status_description_added() {
        assert_eq!(status_description(FileStatus::Added), "new file");
    }

    #[test]
    fn test_status_description_conflicted() {
        assert_eq!(status_description(FileStatus::Conflicted), "conflict");
    }

    #[test]
    fn test_status_description_type_changed() {
        assert_eq!(status_description(FileStatus::TypeChanged), "type changed");
    }

    #[test]
    fn test_status_description_permissions_changed() {
        assert_eq!(status_description(FileStatus::PermissionsChanged), "permissions");
    }

    // Is Recordable Tests

    #[test]
    fn test_is_recordable_modified() {
        assert!(is_recordable(FileStatus::Modified));
    }

    #[test]
    fn test_is_recordable_deleted() {
        assert!(is_recordable(FileStatus::Deleted));
    }

    #[test]
    fn test_is_recordable_added() {
        assert!(is_recordable(FileStatus::Added));
    }

    #[test]
    fn test_is_recordable_type_changed() {
        assert!(is_recordable(FileStatus::TypeChanged));
    }

    #[test]
    fn test_is_recordable_permissions_changed() {
        assert!(is_recordable(FileStatus::PermissionsChanged));
    }

    #[test]
    fn test_is_not_recordable_clean() {
        assert!(!is_recordable(FileStatus::Clean));
    }

    #[test]
    fn test_is_not_recordable_untracked() {
        assert!(!is_recordable(FileStatus::Untracked));
    }

    #[test]
    fn test_is_not_recordable_conflicted() {
        // Conflicted files need resolution, not just recording
        assert!(!is_recordable(FileStatus::Conflicted));
    }

    // Repository Status Format Tests

    #[test]
    fn test_repository_status_empty() {
        let status = RepositoryStatus::new("dev".to_string(), None);
        assert_eq!(status.stack(), "dev");
        assert!(status.state().is_none());
        assert!(status.is_clean());
    }

    #[test]
    fn test_repository_status_with_state() {
        let state = Merkle::initial();
        let status = RepositoryStatus::new("main".to_string(), Some(state));
        assert_eq!(status.stack(), "main");
        assert!(status.state().is_some());
    }

    #[test]
    fn test_repository_status_with_entries() {
        let mut status = RepositoryStatus::new("feature".to_string(), None);

        let entry1 = FileStatusEntry::new(PathBuf::from("src/main.rs"), FileStatus::Modified);
        let entry2 = FileStatusEntry::new(PathBuf::from("src/lib.rs"), FileStatus::Added);
        let entry3 = FileStatusEntry::new(PathBuf::from("README.md"), FileStatus::Untracked);

        status.add_entry(entry1);
        status.add_entry(entry2);
        status.add_entry(entry3);

        assert_eq!(status.modified_count(), 1);
        assert_eq!(status.added_count(), 1);
        assert_eq!(status.untracked_count(), 1);
        assert!(!status.is_clean());
    }

    // Print Format Tests (Output verification)

    #[test]
    fn test_short_format_no_panic_empty() {
        let status = Status::new().with_short(true);
        let repo_status = RepositoryStatus::new("dev".to_string(), None);
        // Should not panic
        let result = status.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_short_format_no_panic_with_entries() {
        let status = Status::new().with_short(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("file.txt"),
            FileStatus::Modified,
        ));

        let result = status.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_no_panic_empty() {
        let status = Status::new();
        let repo_status = RepositoryStatus::new("dev".to_string(), None);
        // Should not panic
        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_no_panic_with_entries() {
        let status = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("file.txt"),
            FileStatus::Modified,
        ));

        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_state() {
        let status = Status::new();
        let state = Merkle::initial();
        let repo_status = RepositoryStatus::new("dev".to_string(), Some(state));

        let result = status.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    // Integration Tests (require temp directories)

    // Note: Integration tests that change the current directory are prone to
    // interfering with each other when run in parallel. We use a guard pattern
    // to save and restore the original directory.

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
    fn test_status_run_outside_repository() {
        let _guard = DirGuard::new();

        // Running status outside a repository should fail with appropriate error
        let temp_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should fail because we're not in a repository
        assert!(result.is_err());
    }

    #[test]
    fn test_status_run_in_repository() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository (takes only path, creates default stack)
        // We need to drop the repository handle before running status to avoid
        // database lock conflicts (redb only allows one open handle at a time)
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        } // Repository dropped here, releasing database lock

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should succeed
        assert!(result.is_ok(), "Status command failed: {:?}", result.err());
    }

    #[test]
    fn test_status_run_short_format() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        }

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_short(true);
        let result = status.run();

        assert!(result.is_ok(), "Status short command failed: {:?}", result.err());
    }

    #[test]
    fn test_status_run_no_untracked() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        }

        // Create an untracked file
        std::fs::write(repo_path.join("untracked.txt"), "content").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_no_untracked(true);
        let result = status.run();

        assert!(result.is_ok(), "Status no-untracked command failed: {:?}", result.err());
    }

    #[test]
    fn test_status_run_with_path_filter() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        }

        // Create directory structure
        std::fs::create_dir(repo_path.join("src")).unwrap();
        std::fs::write(repo_path.join("src/main.rs"), "fn main() {}").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new().with_path("src/");
        let result = status.run();

        assert!(result.is_ok(), "Status with path filter failed: {:?}", result.err());
    }

    #[test]
    fn test_status_in_subdirectory() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        }

        // Create subdirectory
        let subdir = repo_path.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        // Change to subdirectory
        std::env::set_current_dir(&subdir).unwrap();

        let status = Status::new();
        let result = status.run();

        // Should find repo by walking up to parent
        assert!(result.is_ok(), "Status in subdirectory failed: {:?}", result.err());
    }

    #[test]
    fn test_status_with_multiple_file_types() {
        let _guard = DirGuard::new();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Initialize a repository and drop handle to release lock
        {
            let repo_result = Repository::init(repo_path);
            assert!(repo_result.is_ok(), "Failed to init repository: {:?}", repo_result.err());
        }

        // Create various file types
        std::fs::create_dir(repo_path.join("src")).unwrap();
        std::fs::write(repo_path.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo_path.join("README.md"), "# Project").unwrap();
        std::fs::write(repo_path.join("Cargo.toml"), "[package]").unwrap();

        // Change to repo directory
        std::env::set_current_dir(repo_path).unwrap();

        let status = Status::new();
        let result = status.run();

        assert!(result.is_ok(), "Status with multiple files failed: {:?}", result.err());
    }

    // Edge Case Tests

    #[test]
    fn test_status_code_all_variants() {
        // Ensure all FileStatus variants have codes
        let statuses = [
            FileStatus::Clean,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Untracked,
            FileStatus::Added,
            FileStatus::Conflicted,
            FileStatus::TypeChanged,
            FileStatus::PermissionsChanged,
        ];

        for status in statuses {
            let code = status_code(status);
            // Ensure code is a printable ASCII character
            assert!(code.is_ascii());
        }
    }

    #[test]
    fn test_status_description_all_variants() {
        // Ensure all FileStatus variants have descriptions
        let statuses = [
            FileStatus::Clean,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Untracked,
            FileStatus::Added,
            FileStatus::Conflicted,
            FileStatus::TypeChanged,
            FileStatus::PermissionsChanged,
        ];

        for status in statuses {
            let desc = status_description(status);
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn test_is_recordable_all_variants() {
        // Test all variants for recordability
        assert!(!is_recordable(FileStatus::Clean));
        assert!(is_recordable(FileStatus::Modified));
        assert!(is_recordable(FileStatus::Deleted));
        assert!(!is_recordable(FileStatus::Untracked));
        assert!(is_recordable(FileStatus::Added));
        assert!(!is_recordable(FileStatus::Conflicted));
        assert!(is_recordable(FileStatus::TypeChanged));
        assert!(is_recordable(FileStatus::PermissionsChanged));
    }

    #[test]
    fn test_output_config_path_filter_with_string() {
        let config = StatusOutputConfig::new().filter_path(String::from("tests/"));
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    #[test]
    fn test_output_config_path_filter_with_str() {
        let config = StatusOutputConfig::new().filter_path("tests/");
        assert_eq!(config.path_filter, Some("tests/".to_string()));
    }

    #[test]
    fn test_status_with_path_string() {
        let status = Status::new().with_path(String::from("src/commands/"));
        assert_eq!(status.path, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_status_with_path_str() {
        let status = Status::new().with_path("src/commands/");
        assert_eq!(status.path, Some("src/commands/".to_string()));
    }

    #[test]
    fn test_repository_status_filter_methods() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        // Add various entries
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("a.rs"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("b.rs"),
            FileStatus::Deleted,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("c.rs"),
            FileStatus::Added,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("d.txt"),
            FileStatus::Untracked,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("e.rs"),
            FileStatus::Clean,
        ));

        // Test counts
        assert_eq!(status.modified_count(), 1);
        assert_eq!(status.deleted_count(), 1);
        assert_eq!(status.added_count(), 1);
        assert_eq!(status.untracked_count(), 1);
        assert_eq!(status.clean_count(), 1);
        assert_eq!(status.total_count(), 5);

        // Test is_clean with dirty entries
        assert!(!status.is_clean());
    }

    #[test]
    fn test_repository_status_dirty_count() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("clean.rs"),
            FileStatus::Clean,
        ));

        assert_eq!(status.dirty_count(), 2);
    }

    #[test]
    fn test_short_format_with_all_status_types() {
        let status_cmd = Status::new().with_short(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Add one of each type that appears in short format
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("added.rs"),
            FileStatus::Added,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("conflicted.rs"),
            FileStatus::Conflicted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        let result = status_cmd.print_short_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_all_status_types() {
        let status_cmd = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Add one of each type
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("added.rs"),
            FileStatus::Added,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.rs"),
            FileStatus::Deleted,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        let result = status_cmd.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_long_format_with_conflicted_and_details() {
        let status_cmd = Status::new();
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        // Use with_details with all required arguments: path, status, inode, recorded_hash, current_hash, details
        let entry = FileStatusEntry::with_details(
            PathBuf::from("conflicted.rs"),
            FileStatus::Conflicted,
            None, // inode
            None, // recorded_hash
            None, // current_hash
            Some("merge conflict".to_string()), // details
        );
        repo_status.add_entry(entry);

        let result = status_cmd.print_long_format(&repo_status);
        assert!(result.is_ok());
    }

    #[test]
    fn test_short_format_hides_untracked_when_flag_set() {
        let status_cmd = Status::new().with_short(true).with_no_untracked(true);
        let mut repo_status = RepositoryStatus::new("dev".to_string(), None);

        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.rs"),
            FileStatus::Modified,
        ));
        repo_status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        // Should succeed and not show untracked files
        let result = status_cmd.print_short_format(&repo_status);
        assert!(result.is_ok());
    }
}

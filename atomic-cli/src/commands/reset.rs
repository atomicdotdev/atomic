//! The `reset` command for restoring working copy to pristine state.
//!
//! This module implements the `atomic reset` command, which restores the working
//! copy to match the pristine state (last recorded state in the stack).
//!
//! # Usage
//!
//! ```text
//! atomic reset [OPTIONS] [FILES]...
//!
//! Arguments:
//!   [FILES]...  Optional files/directories to reset (default: all)
//!
//! Options:
//!       --stack <STACK>  Reset to a specific stack and switch to it
//!   -n, --dry-run        Preview what would be reset without changes
//!   -f, --force          Force reset even with uncommitted changes
//!   -h, --help           Print help information
//! ```
//!
//! # Behavior
//!
//! The `reset` command:
//! 1. Compares working copy with pristine state
//! 2. Restores files to match the stack state
//! 3. Discards any unrecorded modifications
//! 4. Optionally switches to a different stack
//!
//! **Warning**: Reset discards uncommitted changes permanently.
//!
//! # Examples
//!
//! Discard all uncommitted changes:
//! ```text
//! $ atomic reset --force
//! Resetting working copy...
//! ✓ Reset 3 files
//! ```
//!
//! Reset specific files:
//! ```text
//! $ atomic reset src/main.rs
//! Resetting: src/main.rs
//! ✓ Reset 1 file
//! ```
//!
//! Switch stacks:
//! ```text
//! $ atomic reset --stack main
//! Switching to stack 'main'...
//! ✓ Reset working copy to stack 'main'
//! ```
//!
//! Dry run (preview):
//! ```text
//! $ atomic reset --dry-run src/
//! Would reset: src/main.rs
//! Would reset: src/lib.rs
//! (dry run - no changes made)
//! ```

use std::path::{Path, PathBuf};

use atomic_repository::{FileStatus, Repository, StatusOptions};
use clap::Parser;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Reset Command

/// Reset the working copy to the last recorded state.
///
/// The `reset` command restores the working copy to match the pristine state
/// (the last recorded state in the stack). This discards any uncommitted
/// changes.
///
/// # Behavior
///
/// - Without arguments: Resets entire working copy
/// - With file arguments: Resets only specified files
/// - With `--stack`: Switches to that stack and resets
///
/// # Warning
///
/// Reset is destructive - uncommitted changes cannot be recovered.
/// Use `--dry-run` to preview changes first.
#[derive(Parser, Debug, Clone)]
#[command(name = "reset")]
pub struct Reset {
    /// Files or directories to reset.
    ///
    /// If not specified, resets the entire working copy.
    #[arg(value_name = "FILES")]
    pub files: Vec<String>,

    /// Reset to a specific stack and switch to it.
    ///
    /// The working copy will be updated to match the specified stack's
    /// pristine state.
    #[arg(long)]
    pub stack: Option<String>,

    /// Dry run - show what would be reset without doing it.
    ///
    /// For a single file, outputs the pristine content to stdout.
    /// For multiple files, lists what would be reset.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force reset even if there are uncommitted changes.
    ///
    /// Without this flag, reset will warn and abort if there are
    /// uncommitted changes (safety measure).
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Reset {
    /// Create a new Reset command with default settings.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            stack: None,
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set files to reset.
    pub fn with_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.files = files.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Builder: set the stack to reset to.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Check if this is a partial reset (specific files, not whole working copy).
    pub fn is_partial_reset(&self) -> bool {
        !self.files.is_empty()
    }

    /// Normalize a path relative to the repository root.
    ///
    /// If the path is absolute and under the repo root, strips the prefix.
    /// Otherwise returns the path as-is.
    pub fn normalize_path(&self, repo_root: &std::path::Path, path: &str) -> CliResult<String> {
        let p = std::path::Path::new(path);
        // On Windows, Path::is_absolute() returns false for Unix-style "/foo"
        // paths (they're drive-relative). Treat a leading '/' as absolute on
        // all platforms so cross-platform behaviour is consistent.
        let looks_absolute = p.is_absolute() || path.starts_with('/');
        if looks_absolute {
            match p.strip_prefix(repo_root) {
                Ok(rel) => Ok(rel.to_string_lossy().to_string()),
                Err(_) => Err(CliError::PathOutsideRepository {
                    path: std::path::PathBuf::from(path),
                }),
            }
        } else {
            Ok(path.to_string())
        }
    }

    /// Check if a path matches any of the specified file filters.
    fn matches_filter(&self, path: &str) -> bool {
        if self.files.is_empty() {
            return true;
        }

        for filter in &self.files {
            let filter_normalized = filter.trim_end_matches('/');
            if path == filter_normalized
                || path.starts_with(&format!("{}/", filter_normalized))
                || filter_normalized.ends_with('/') && path.starts_with(filter_normalized)
            {
                return true;
            }
        }
        false
    }

    /// Get files that need to be reset based on status.
    fn get_files_to_reset(&self, repo: &Repository) -> CliResult<Vec<PathBuf>> {
        let status = repo
            .status(StatusOptions::default())
            .map_err(CliError::Repository)?;

        let mut files_to_reset = Vec::new();

        for entry in status.entries() {
            // Only reset modified, deleted files (not untracked)
            match entry.status() {
                FileStatus::Modified | FileStatus::Deleted => {
                    let path_str = entry.path().to_string_lossy();
                    if self.matches_filter(&path_str) {
                        files_to_reset.push(entry.path().to_path_buf());
                    }
                }
                _ => {}
            }
        }

        Ok(files_to_reset)
    }

    /// Execute dry-run for a single file - output pristine content to stdout.
    fn dry_run_single_file(&self, repo: &Repository, path: &str) -> CliResult<()> {
        // Get file content from pristine
        let content = repo.get_file_content(Path::new(path)).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to get file content: {}", e))
        })?;

        match content {
            Some(bytes) => {
                // Output to stdout
                use std::io::Write;
                std::io::stdout()
                    .write_all(&bytes)
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to write: {}", e)))?;
                Ok(())
            }
            None => Err(CliError::FileNotFound {
                path: PathBuf::from(path),
            }),
        }
    }

    /// Reset a single file to pristine state.
    fn reset_file(&self, repo: &Repository, repo_root: &Path, path: &Path) -> CliResult<bool> {
        // Get content from pristine
        let content = repo.get_file_content(path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to get file content: {}", e))
        })?;

        let full_path = repo_root.join(path);

        match content {
            Some(bytes) => {
                // Ensure parent directory exists
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        CliError::Internal(anyhow::anyhow!("Failed to create directory: {}", e))
                    })?;
                }

                // Write content
                std::fs::write(&full_path, bytes).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to write file: {}", e))
                })?;

                Ok(true)
            }
            None => {
                // File doesn't exist in pristine - it was added but not recorded
                // For reset, we leave it as-is (it's untracked)
                Ok(false)
            }
        }
    }

    /// Switch to a different stack.
    fn switch_stack(&self, repo: &mut Repository, stack_name: &str) -> CliResult<()> {
        // Check stack exists
        if !repo
            .stack_exists(stack_name)
            .map_err(CliError::Repository)?
        {
            return Err(CliError::StackNotFound {
                name: stack_name.to_string(),
            });
        }

        // Switch stack
        repo.set_current_stack(stack_name)
            .map_err(CliError::Repository)?;

        Ok(())
    }
}

impl Default for Reset {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Reset {
    /// Execute the reset command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Check for uncommitted changes (unless --force)
    /// 3. If --stack, switch to that stack
    /// 4. Determine files to reset
    /// 5. If --dry-run, preview changes
    /// 6. Otherwise, reset files to pristine state
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Get current status
        let status = repo
            .status(StatusOptions::default())
            .map_err(CliError::Repository)?;

        let has_changes = !status.is_clean();

        // Check for uncommitted changes (unless force)
        if has_changes && !self.force && !self.dry_run {
            print_warning("Cannot reset: there are uncommitted changes.");
            print_hint("Use 'atomic diff' to see changes");
            print_hint("Use 'atomic reset --force' to discard changes");
            print_hint("Use 'atomic record' to save changes first");
            return Err(CliError::Internal(anyhow::anyhow!(
                "Uncommitted changes would be lost"
            )));
        }

        // Handle stack switching
        if let Some(ref stack_name) = self.stack {
            if !self.dry_run {
                println!("Switching to stack '{}'...", stack_name);
                self.switch_stack(&mut repo, stack_name)?;
            } else {
                println!("Would switch to stack '{}'", stack_name);
            }
        }

        // Get files to reset
        let files_to_reset = self.get_files_to_reset(&repo)?;

        // Handle dry-run for single file (output content)
        if self.dry_run && self.files.len() == 1 && files_to_reset.len() <= 1 {
            let path = &self.files[0];
            return self.dry_run_single_file(&repo, path);
        }

        // Dry run mode - just show what would happen
        if self.dry_run {
            if files_to_reset.is_empty() {
                println!("Nothing to reset - working copy is clean");
            } else {
                for path in &files_to_reset {
                    println!("Would reset: {}", path.display());
                }
                println!();
                print_hint(&format!(
                    "(dry run - {} would be reset)",
                    format_count(files_to_reset.len(), "file")
                ));
            }
            return Ok(());
        }

        // Check if there's anything to reset
        if files_to_reset.is_empty() {
            if let Some(stack) = &self.stack {
                print_success(&format!(
                    "Switched to stack '{}' (working copy already clean)",
                    stack
                ));
            } else {
                println!("Nothing to reset - working copy is clean");
            }
            return Ok(());
        }

        // Perform reset
        println!("Resetting working copy...");

        let mut reset_count = 0;
        let mut error_count = 0;

        for path in &files_to_reset {
            let path_display = path.display();

            match self.reset_file(&repo, &repo_root, path) {
                Ok(true) => {
                    println!("  Reset: {}", path_display);
                    reset_count += 1;
                }
                Ok(false) => {
                    // File not in pristine (was newly added)
                    // We don't delete untracked files by default
                }
                Err(e) => {
                    print_warning(&format!("Failed to reset '{}': {}", path_display, e));
                    error_count += 1;
                }
            }
        }

        // Summary
        println!();
        if reset_count > 0 {
            if let Some(ref stack_name) = self.stack {
                print_success(&format!(
                    "Reset {} to stack '{}'",
                    format_count(reset_count, "file"),
                    stack_name
                ));
            } else {
                print_success(&format!("Reset {}", format_count(reset_count, "file")));
            }
        }

        if error_count > 0 {
            print_warning(&format!(
                "{} could not be reset",
                format_count(error_count, "file")
            ));
        }

        Ok(())
    }
}

/// Format a count with singular/plural word.
fn format_count(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{} {}", count, word)
    } else {
        format!("{} {}s", count, word)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Builder Tests

    #[test]
    fn test_reset_new() {
        let cmd = Reset::new();
        assert!(cmd.files.is_empty());
        assert!(cmd.stack.is_none());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_reset_default() {
        let cmd = Reset::default();
        assert!(cmd.files.is_empty());
        assert!(cmd.stack.is_none());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_reset_with_files() {
        let cmd = Reset::new().with_files(vec!["file.txt".to_string(), "src/".to_string()]);
        assert_eq!(cmd.files.len(), 2);
        assert_eq!(cmd.files[0], "file.txt");
        assert_eq!(cmd.files[1], "src/");
    }

    #[test]
    fn test_reset_with_stack() {
        let cmd = Reset::new().with_stack("main");
        assert_eq!(cmd.stack, Some("main".to_string()));
    }

    #[test]
    fn test_reset_with_dry_run() {
        let cmd = Reset::new().with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_reset_with_force() {
        let cmd = Reset::new().with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_reset_builder_chain() {
        let cmd = Reset::new()
            .with_files(vec!["src/main.rs".to_string()])
            .with_stack("feature")
            .with_dry_run(true)
            .with_force(true);

        assert_eq!(cmd.files, vec!["src/main.rs"]);
        assert_eq!(cmd.stack, Some("feature".to_string()));
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Partial Reset Tests

    #[test]
    fn test_is_partial_reset_empty() {
        let cmd = Reset::new();
        assert!(!cmd.is_partial_reset());
    }

    #[test]
    fn test_is_partial_reset_with_files() {
        let cmd = Reset::new().with_files(vec!["file.txt".to_string()]);
        assert!(cmd.is_partial_reset());
    }

    // Filter Tests

    #[test]
    fn test_matches_filter_empty() {
        let cmd = Reset::new();
        assert!(cmd.matches_filter("any/path.rs"));
    }

    #[test]
    fn test_matches_filter_exact() {
        let cmd = Reset::new().with_files(vec!["src/main.rs".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("src/lib.rs"));
    }

    #[test]
    fn test_matches_filter_directory() {
        let cmd = Reset::new().with_files(vec!["src/".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("src/utils/helpers.rs"));
        assert!(!cmd.matches_filter("tests/test.rs"));
    }

    #[test]
    fn test_matches_filter_directory_without_slash() {
        let cmd = Reset::new().with_files(vec!["src".to_string()]);
        assert!(cmd.matches_filter("src"));
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(!cmd.matches_filter("srcfile.rs"));
    }

    #[test]
    fn test_matches_filter_multiple() {
        let cmd = Reset::new().with_files(vec!["src/".to_string(), "README.md".to_string()]);
        assert!(cmd.matches_filter("src/main.rs"));
        assert!(cmd.matches_filter("README.md"));
        assert!(!cmd.matches_filter("Cargo.toml"));
    }

    // Normalize Path Tests

    #[test]
    fn test_normalize_relative_path() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "src/file.rs").unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_inside_repo() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let abs = temp.path().join("src/file.rs");
        let result = cmd
            .normalize_path(temp.path(), abs.to_str().unwrap())
            .unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_outside_repo() {
        let cmd = Reset::new();
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "/other/path/file.rs");
        assert!(result.is_err());
    }

    // Format Tests

    #[test]
    fn test_format_count_singular() {
        assert_eq!(format_count(1, "file"), "1 file");
    }

    #[test]
    fn test_format_count_plural() {
        assert_eq!(format_count(2, "file"), "2 files");
    }

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "file"), "0 files");
    }

    // Clone Tests

    #[test]
    fn test_reset_clone() {
        let cmd = Reset::new()
            .with_files(vec!["file.txt".to_string()])
            .with_stack("main")
            .with_force(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.files, cmd.files);
        assert_eq!(cloned.stack, cmd.stack);
        assert_eq!(cloned.force, cmd.force);
    }
}

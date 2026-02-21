//! The `remove` command for untracking files in the repository.
//!
//! This module implements the `atomic remove` command, which removes files and
//! directories from the repository's tracking system. This is the inverse of
//! `atomic add`.
//!
//! # Usage
//!
//! ```text
//! atomic remove [OPTIONS] <PATHS>...
//!
//! Arguments:
//!   <PATHS>...  Files or directories to remove from tracking
//!
//! Options:
//!       --keep          Keep files on disk (don't delete)
//!   -r, --recursive     Recursively remove directory contents (default)
//!       --no-recursive  Don't recursively remove directory contents
//!   -n, --dry-run       Show what would be removed without doing it
//!   -f, --force         Force remove even if file has uncommitted changes
//!   -h, --help          Print help information
//! ```
//!
//! # Behavior
//!
//! By default, `remove` does two things:
//! 1. Removes the file from tracking (undoes `add`)
//! 2. Deletes the file from the working copy
//!
//! Use `--keep` to only remove from tracking without deleting.
//!
//! # Examples
//!
//! Remove a file from tracking (keeps on disk):
//! ```text
//! $ atomic remove old_file.txt
//! Untracking: old_file.txt
//! ✓ Removed 1 file
//! ```
//!
//! Remove from tracking AND delete from disk:
//! ```text
//! $ atomic remove --delete old_file.txt
//! Removing: old_file.txt
//! ✓ Removed 1 file
//! ```
//!
//! Remove a directory recursively:
//! ```text
//! $ atomic remove old_code/
//! Removing: old_code/main.rs
//! Removing: old_code/lib.rs
//! ✓ Removed 2 files
//! ```

use std::path::PathBuf;

use clap::Parser;

use atomic_repository::tracking::TrackingOptions;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Remove Command

/// Remove files from tracking.
///
/// The `remove` command stops tracking files in the repository. By default,
/// files are kept on disk (only untracked). Use `--delete` to also delete
/// from the working copy.
///
/// # Behavior
///
/// - Files: Removed from tracking, kept on disk by default
/// - Directories: Removed recursively by default
/// - Untracked files: Error unless `--force` is used
///
/// # Options
///
/// - `--delete`: Also delete files from disk
/// - `--recursive` / `-r`: Recursively remove directory contents (default: true)
/// - `--no-recursive`: Don't recursively remove directory contents
/// - `--dry-run` / `-n`: Preview what would be removed
/// - `--force` / `-f`: Force remove even if not tracked
#[derive(Parser, Debug, Clone)]
#[command(name = "remove")]
pub struct Remove {
    /// Files or directories to remove from tracking.
    ///
    /// Paths can be relative or absolute. Directories will be removed
    /// recursively by default.
    #[arg(value_name = "PATHS", required = true)]
    pub paths: Vec<String>,

    /// Also delete files from disk.
    ///
    /// Without this flag, files are kept on the working copy (only
    /// untracked).  Use this when you want to both stop versioning
    /// and remove the file from the working copy.
    #[arg(long)]
    pub delete: bool,

    /// Keep files on disk but stop tracking them.
    ///
    /// This is now the default behaviour. Kept for backward compatibility.
    #[arg(long, hide = true)]
    pub keep: bool,

    /// Recursively remove directory contents.
    ///
    /// When removing a directory, also remove all files within it.
    /// This is the default behavior.
    #[arg(short = 'r', long = "recursive", default_value = "true")]
    pub recursive: bool,

    /// Don't recursively remove directory contents.
    ///
    /// When set, only the specified path is removed, not its contents.
    #[arg(long = "no-recursive", conflicts_with = "recursive")]
    pub no_recursive: bool,

    /// Dry run - show what would be removed without doing it.
    ///
    /// When enabled, displays the files that would be removed but doesn't
    /// actually modify the repository or delete files.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force remove even if the file is not tracked.
    ///
    /// Normally, attempting to remove an untracked file results in an error.
    /// This flag suppresses that error.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Remove {
    /// Create a new Remove command with default settings.
    pub fn new(paths: Vec<String>) -> Self {
        Self {
            paths,
            delete: false,
            keep: false,
            recursive: true,
            no_recursive: false,
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set the delete flag.
    pub fn with_delete(mut self, delete: bool) -> Self {
        self.delete = delete;
        self
    }

    /// Builder: set the keep flag (legacy, now the default).
    pub fn with_keep(mut self, keep: bool) -> Self {
        self.keep = keep;
        self
    }

    /// Builder: set the recursive flag.
    pub fn with_recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        if !recursive {
            self.no_recursive = true;
        }
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

    /// Convert to TrackingOptions.
    fn to_tracking_options(&self) -> TrackingOptions {
        let mut options = TrackingOptions::default()
            .with_recursive(self.recursive && !self.no_recursive)
            .with_force(self.force);
        options.dry_run = self.dry_run;
        options
    }

    /// Delete a file from the filesystem.
    fn delete_file(&self, repo_root: &std::path::Path, path: &str) -> std::io::Result<()> {
        let full_path = repo_root.join(path);
        if full_path.is_dir() {
            std::fs::remove_dir_all(&full_path)
        } else if full_path.exists() {
            std::fs::remove_file(&full_path)
        } else {
            Ok(()) // Already doesn't exist
        }
    }

    /// Format the output message based on mode.
    fn format_action(&self) -> &'static str {
        if self.keep {
            "Untracking"
        } else {
            "Removing"
        }
    }
}

impl Default for Remove {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Command for Remove {
    /// Execute the remove command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. For each path:
    ///    a. Remove from tracking
    ///    b. If not --keep, delete from disk
    /// 3. Display results
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        let options = self.to_tracking_options();
        let action = self.format_action();

        let mut total_removed = 0;
        let mut total_errors = 0;
        let mut files_to_delete: Vec<PathBuf> = Vec::new();

        // Process each path
        for path in &self.paths {
            // Normalize path relative to repo root
            let normalized = if std::path::Path::new(path).is_absolute() {
                match std::path::Path::new(path).strip_prefix(&repo_root) {
                    Ok(rel) => rel.to_string_lossy().to_string(),
                    Err(_) => {
                        print_warning(&format!("Path outside repository: {}", path));
                        total_errors += 1;
                        continue;
                    }
                }
            } else {
                path.clone()
            };

            if self.dry_run {
                println!("Would remove: {}", normalized);
            } else {
                println!("{}: {}", action, normalized);
            }

            // Remove from tracking
            match repo.remove(&normalized, options.clone()) {
                Ok(stats) => {
                    total_removed += stats.files_removed;

                    // Collect files to delete if not keeping
                    if self.delete && !self.keep && !self.dry_run {
                        files_to_delete.push(PathBuf::from(&normalized));
                    }
                }
                Err(e) => {
                    if !self.force {
                        print_warning(&format!("Failed to remove '{}': {}", normalized, e));
                        total_errors += 1;
                    }
                }
            }
        }

        // Delete files from disk if not keeping
        if !self.keep && !self.dry_run {
            for path in &files_to_delete {
                if let Err(e) = self.delete_file(&repo_root, &path.to_string_lossy()) {
                    print_warning(&format!("Failed to delete '{}': {}", path.display(), e));
                }
            }
        }

        // Display summary
        println!();
        if self.dry_run {
            print_hint(&format!(
                "Would remove {} (dry run - no changes made)",
                format_count(total_removed, "file")
            ));
        } else if total_removed > 0 {
            let verb = if self.keep { "Untracked" } else { "Removed" };
            print_success(&format!("{} {}", verb, format_count(total_removed, "file")));
        } else if total_errors == 0 {
            print_hint("Nothing to remove");
        }

        if total_errors > 0 {
            print_warning(&format!(
                "{} with errors",
                format_count(total_errors, "path")
            ));
        }

        // Remind user to record
        if total_removed > 0 && !self.dry_run {
            println!();
            print_hint("Run 'atomic record' to save these changes");
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
    fn test_remove_new() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        assert_eq!(cmd.paths, vec!["file.txt"]);
        assert!(!cmd.keep);
        assert!(cmd.recursive);
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_remove_default() {
        let cmd = Remove::default();
        assert!(cmd.paths.is_empty());
        assert!(!cmd.keep);
        assert!(cmd.recursive);
    }

    #[test]
    fn test_remove_with_keep() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_keep(true);
        assert!(cmd.keep);
    }

    #[test]
    fn test_remove_with_recursive() {
        let cmd = Remove::new(vec!["dir/".to_string()]).with_recursive(false);
        assert!(!cmd.recursive);
        assert!(cmd.no_recursive);
    }

    #[test]
    fn test_remove_with_dry_run() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_remove_with_force() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_remove_builder_chain() {
        let cmd = Remove::new(vec!["file.txt".to_string()])
            .with_keep(true)
            .with_dry_run(true)
            .with_force(true);

        assert!(cmd.keep);
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Options Conversion Tests

    #[test]
    fn test_to_tracking_options_default() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        let options = cmd.to_tracking_options();
        // TrackingOptions doesn't expose fields, so we just verify it doesn't panic
        let _ = options;
    }

    #[test]
    fn test_to_tracking_options_non_recursive() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_recursive(false);
        let options = cmd.to_tracking_options();
        let _ = options;
    }

    // Format Tests

    #[test]
    fn test_format_action_remove() {
        let cmd = Remove::new(vec!["file.txt".to_string()]);
        assert_eq!(cmd.format_action(), "Removing");
    }

    #[test]
    fn test_format_action_untrack() {
        let cmd = Remove::new(vec!["file.txt".to_string()]).with_keep(true);
        assert_eq!(cmd.format_action(), "Untracking");
    }

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

    // Multiple Paths Tests

    #[test]
    fn test_remove_multiple_paths() {
        let cmd = Remove::new(vec![
            "file1.txt".to_string(),
            "file2.txt".to_string(),
            "dir/".to_string(),
        ]);
        assert_eq!(cmd.paths.len(), 3);
    }

    #[test]
    fn test_remove_clone() {
        let cmd = Remove::new(vec!["file.txt".to_string()])
            .with_keep(true)
            .with_force(true);
        let cloned = cmd.clone();
        assert_eq!(cloned.paths, cmd.paths);
        assert_eq!(cloned.keep, cmd.keep);
        assert_eq!(cloned.force, cmd.force);
    }
}

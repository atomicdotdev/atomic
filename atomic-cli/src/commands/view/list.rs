//! The `view list` command for listing all views.
//!
//! This module implements the `atomic view list` command, which shows all
//! views in the repository. The current view is marked with an asterisk (*).
//!
//! # Usage
//!
//! ```text
//! atomic view list [OPTIONS]
//!
//! Options:
//!   -v, --verbose  Show additional details (state hash, change count)
//!   -h, --help     Print help information
//! ```
//!
//! # Examples
//!
//! List all views:
//! ```text
//! $ atomic view list
//!   dev
//! * feature-auth
//!   release-1.0
//! ```
//!
//! List with verbose output:
//! ```text
//! $ atomic view list --verbose
//!   dev           (0 changes)   state: 2AAAAAAAA...
//! * feature-auth  (3 changes)   state: XYZABCDEF...
//!   release-1.0   (10 changes)  state: 123456789...
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{hint, view as style_view};

#[cfg(test)]
use std::path::PathBuf;

// List Command

/// List all views.
///
/// Shows all views in the repository with the current view marked
/// with an asterisk (*).
#[derive(Parser, Debug, Default)]
#[command(name = "list")]
pub struct List {
    /// Show additional details (state hash, change count).
    ///
    /// When enabled, displays the Merkle state hash and number of
    /// changes for each view.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

impl List {
    /// Create a new List command with default settings.
    pub fn new() -> Self {
        Self { verbose: false }
    }

    /// Builder: set the verbose flag.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        // Find the repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Get list of views
        let views = repo.list_views().map_err(CliError::Repository)?;
        let current = repo.current_view();

        if views.is_empty() {
            println!(
                "{}",
                hint("No views found. Use 'atomic view create <name>' to create one.")
            );
            return Ok(());
        }

        // Sort views alphabetically, but keep current view considerations
        let mut sorted_views = views;
        sorted_views.sort();

        // Calculate padding for alignment in verbose mode
        let max_name_len = sorted_views.iter().map(|s| s.len()).max().unwrap_or(0);

        for view in sorted_views {
            let is_current = view == current;
            let marker = if is_current { "*" } else { " " };

            if self.verbose {
                // Get view info for verbose output
                match repo.get_view_info(&view) {
                    Ok(info) => {
                        let change_word = if info.change_count == 1 {
                            "change"
                        } else {
                            "changes"
                        };
                        let kind_tag = match info.kind_label() {
                            "draft" => "[draft]",
                            _ => "[shared]",
                        };
                        let parent_info = match &info.parent_name {
                            Some(p) => format!("  parent: {}", style_view(p)),
                            None => String::new(),
                        };
                        println!(
                            "{} {:<width$}  {:<10}  ({} {})  state: {}{}",
                            marker,
                            style_view(&view),
                            kind_tag,
                            info.change_count,
                            change_word,
                            info.state_short(),
                            parent_info,
                            width = max_name_len
                        );
                    }
                    Err(_) => {
                        // Fall back to simple output if we can't get info
                        println!("{} {}", marker, style_view(&view));
                    }
                }
            } else {
                println!("{} {}", marker, style_view(&view));
            }
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Directory Guard for Safe Current Dir Changes
    // -------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_default() {
        let cmd = List::default();
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_new() {
        let cmd = List::new();
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_with_verbose() {
        let cmd = List::new().with_verbose(true);
        assert!(cmd.verbose);
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_list_default_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_multiple_views() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create some views, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature-a").unwrap();
            repo.create_view("feature-b").unwrap();
            repo.create_view("release-1.0").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_verbose() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views with verbose output
        let cmd = List::new().with_verbose(true);
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_shows_current_marker() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create views, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("other").unwrap();
            // Verify current view is dev
            assert_eq!(repo.current_view(), "dev");
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views - should mark "dev" as current
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_after_switch() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature").unwrap();
            repo.align_to_view("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List views - should mark "feature" as current
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on feature
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "feature");
    }
}

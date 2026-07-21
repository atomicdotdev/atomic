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
//!   -s, --short    Show only view names (no metadata)
//!   -h, --help     Print help information
//! ```
//!
//! # Examples
//!
//! List all views (default — shows metadata):
//! ```text
//! $ atomic view list
//!   dev              [shared]    (3 changes)                 state: 2AAAAAAAA...
//! * feature-auth     [draft]     (2 changes, 3 inherited)   state: XYZABCDEF...  parent: dev
//! ```
//!
//! Short listing (names only):
//! ```text
//! $ atomic view list --short
//!   dev
//! * feature-auth
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
/// with an asterisk (*). By default, displays metadata (scope, change
/// count, state hash, parent). Use `--short` for names only.
#[derive(Parser, Debug, Default)]
#[command(name = "list")]
pub struct List {
    /// Show only view names (no metadata).
    #[arg(long, short = 's')]
    pub short: bool,

    /// Show additional details (state hash, change count).
    ///
    /// This is now the default behavior. Kept for backward compatibility.
    #[arg(long, short = 'v', hide = true)]
    pub verbose: bool,
}

impl List {
    /// Create a new List command with default settings.
    pub fn new() -> Self {
        Self {
            short: false,
            verbose: false,
        }
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

        // Calculate padding for alignment
        let max_name_len = sorted_views.iter().map(|s| s.len()).max().unwrap_or(0);

        for view in sorted_views {
            let is_current = view == current;
            let marker = if is_current { "*" } else { " " };

            if self.short {
                println!("{} {}", marker, style_view(&view));
            } else {
                match repo.get_view_info(&view) {
                    Ok(info) => {
                        let kind_tag = match info.kind_label() {
                            "draft" => "[draft]",
                            _ => "[shared]",
                        };
                        let parent_info = match &info.parent_name {
                            Some(p) => format!("  parent: {}", style_view(p)),
                            None => String::new(),
                        };
                        // Show own changes for views with a parent;
                        // show total for root views.
                        let change_display = if info.parent_name.is_some() {
                            let own = info.own_change_count;
                            let inherited = info.change_count.saturating_sub(info.own_change_count);
                            if own == 1 {
                                format!("({} change, {} inherited)", own, inherited)
                            } else {
                                format!("({} changes, {} inherited)", own, inherited)
                            }
                        } else if info.change_count == 1 {
                            format!("({} change)", info.change_count)
                        } else {
                            format!("({} changes)", info.change_count)
                        };
                        println!(
                            "{} {:<width$}  {:<10}  {}  state: {}{}",
                            marker,
                            style_view(&view),
                            kind_tag,
                            change_display,
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
        assert!(!cmd.short);
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_new() {
        let cmd = List::new();
        assert!(!cmd.short);
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

        // List views (default = verbose)
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

        // List views with verbose output (same as default now)
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

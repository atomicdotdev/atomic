//! The `stack list` command for listing all stacks.
//!
//! This module implements the `atomic stack list` command, which shows all
//! stacks in the repository. The current stack is marked with an asterisk (*).
//!
//! # Usage
//!
//! ```text
//! atomic stack list [OPTIONS]
//!
//! Options:
//!   -v, --verbose  Show additional details (state hash, change count)
//!   -h, --help     Print help information
//! ```
//!
//! # Examples
//!
//! List all stacks:
//! ```text
//! $ atomic stack list
//!   dev
//! * feature-auth
//!   release-1.0
//! ```
//!
//! List with verbose output:
//! ```text
//! $ atomic stack list --verbose
//!   dev           (0 changes)   state: 2AAAAAAAA...
//! * feature-auth  (3 changes)   state: XYZABCDEF...
//!   release-1.0   (10 changes)  state: 123456789...
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{hint, stack as style_stack};

#[cfg(test)]
use std::path::PathBuf;

// List Command

/// List all stacks.
///
/// Shows all stacks in the repository with the current stack marked
/// with an asterisk (*).
#[derive(Parser, Debug, Default)]
#[command(name = "list")]
pub struct List {
    /// Show additional details (state hash, change count).
    ///
    /// When enabled, displays the Merkle state hash and number of
    /// changes for each stack.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

impl List {
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        // Find the repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => {
                CliError::RepositoryNotFound {
                    searched_path: path.into(),
                }
            }
            other => CliError::Repository(other),
        })?;

        // Get list of stacks
        let stacks = repo.list_stacks().map_err(CliError::Repository)?;
        let current = repo.current_stack();

        if stacks.is_empty() {
            println!("{}", hint("No stacks found. Use 'atomic stack new <name>' to create one."));
            return Ok(());
        }

        // Sort stacks alphabetically, but keep current stack considerations
        let mut sorted_stacks = stacks;
        sorted_stacks.sort();

        // Calculate padding for alignment in verbose mode
        let max_name_len = sorted_stacks.iter().map(|s| s.len()).max().unwrap_or(0);

        for stack in sorted_stacks {
            let is_current = stack == current;
            let marker = if is_current { "*" } else { " " };

            if self.verbose {
                // Get stack info for verbose output
                match repo.get_stack_info(&stack) {
                    Ok(info) => {
                        let change_word = if info.change_count == 1 { "change" } else { "changes" };
                        println!(
                            "{} {:<width$}  ({} {})  state: {}",
                            marker,
                            style_stack(&stack),
                            info.change_count,
                            change_word,
                            info.state_short(),
                            width = max_name_len
                        );
                    }
                    Err(_) => {
                        // Fall back to simple output if we can't get info
                        println!("{} {}", marker, style_stack(&stack));
                    }
                }
            } else {
                println!("{} {}", marker, style_stack(&stack));
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
    fn test_list_default_stack() {
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

        // List stacks
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_multiple_stacks() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create some stacks, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("feature-a").unwrap();
            repo.create_stack("feature-b").unwrap();
            repo.create_stack("release-1.0").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List stacks
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

        // Initialize a repository and create a stack, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List stacks with verbose output
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

        // Initialize a repository and create stacks, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("other").unwrap();
            // Verify current stack is dev
            assert_eq!(repo.current_stack(), "dev");
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List stacks - should mark "dev" as current
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

        // Initialize a repository and create a stack, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("feature").unwrap();
            repo.set_current_stack("feature").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List stacks - should mark "feature" as current
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on feature
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_stack(), "feature");
    }
}

//! The `tag list` command for listing all tags.
//!
//! This module implements the `atomic tag list` command, which shows all
//! tags in the repository. Tags can be filtered and sorted, and additional
//! details can be shown with the `--verbose` flag.
//!
//! # Usage
//!
//! ```text
//! atomic tag list [OPTIONS]
//!
//! Options:
//!   -v, --verbose         Show additional details (state, sequence, date)
//!   -s, --stack <STACK>   Filter tags by stack
//!   -p, --pattern <PAT>   Filter tags by name pattern (glob)
//!   --annotated-only      Show only annotated tags
//!   -h, --help            Print help information
//! ```
//!
//! # Examples
//!
//! List all tags:
//! ```text
//! $ atomic tag list
//! v1.0.0
//! v1.1.0
//! v2.0.0-beta
//! ```
//!
//! List tags with details:
//! ```text
//! $ atomic tag list --verbose
//! v1.0.0      (seq: 42)   state: ABCD1234...  2024-01-15
//! v1.1.0      (seq: 58)   state: EFGH5678...  2024-02-20
//! v2.0.0-beta (seq: 73)   state: IJKL9012...  2024-03-10
//! ```

use clap::Parser;

use atomic_repository::{Repository, TagFilter};
use atomic_core::types::Base32;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint};

#[cfg(test)]
use std::path::PathBuf;

// List Command

/// List all tags.
///
/// Shows all tags in the repository, optionally with additional details
/// like state, sequence number, and creation date.
#[derive(Parser, Debug, Default)]
#[command(name = "list")]
pub struct List {
    /// Show additional details (state, sequence, date).
    ///
    /// When enabled, displays the Merkle state, sequence number,
    /// and creation timestamp for each tag.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Filter tags by stack.
    ///
    /// Only show tags that belong to the specified stack.
    #[arg(long, short = 's', value_name = "STACK")]
    pub stack: Option<String>,

    /// Filter tags by name pattern.
    ///
    /// Supports glob patterns like `v*` or `release-*`.
    #[arg(long, short = 'p', value_name = "PATTERN")]
    pub pattern: Option<String>,

    /// Show only annotated tags.
    ///
    /// Filters out lightweight tags, showing only those with
    /// messages and/or author information.
    #[arg(long)]
    pub annotated_only: bool,
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

        // Build filter
        let mut filter = TagFilter::new();

        if let Some(stack) = &self.stack {
            filter = filter.stack(stack);
        }

        if let Some(pattern) = &self.pattern {
            filter = filter.pattern(pattern);
        }

        if self.annotated_only {
            filter = filter.annotated_only();
        }

        // Get tags
        let tags = repo.list_tags_filtered(&filter).map_err(CliError::Repository)?;

        if tags.is_empty() {
            println!("{}", hint("No tags found. Use 'atomic tag create <name>' to create one."));
            return Ok(());
        }

        // Calculate padding for alignment in verbose mode
        let max_name_len = tags.iter().map(|t| t.name.len()).max().unwrap_or(0);

        for tag in tags {
            if self.verbose {
                let state_short = {
                    let full = tag.state.to_base32();
                    if full.len() > 12 {
                        format!("{}...", &full[..12])
                    } else {
                        full
                    }
                };
                let date = tag.timestamp.format("%Y-%m-%d").to_string();
                let annotated_marker = if tag.is_annotated() { "*" } else { " " };

                println!(
                    "{}{:<width$}  (seq: {:>4})  state: {}  {}",
                    annotated_marker,
                    emphasis(&tag.name),
                    tag.sequence,
                    state_short,
                    date,
                    width = max_name_len
                );
            } else {
                let annotated_marker = if tag.is_annotated() { "*" } else { " " };
                println!("{}{}", annotated_marker, emphasis(&tag.name));
            }
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::TagOptions;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_default() {
        let cmd = List::default();
        assert!(!cmd.verbose);
        assert!(cmd.stack.is_none());
        assert!(cmd.pattern.is_none());
        assert!(!cmd.annotated_only);
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

    #[test]
    fn test_with_stack() {
        let cmd = List::new().with_stack("release");
        assert_eq!(cmd.stack, Some("release".to_string()));
    }

    #[test]
    fn test_with_pattern() {
        let cmd = List::new().with_pattern("v*");
        assert_eq!(cmd.pattern, Some("v*".to_string()));
    }

    #[test]
    fn test_with_annotated_only() {
        let cmd = List::new().with_annotated_only(true);
        assert!(cmd.annotated_only);
    }

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
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_list_empty() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository without any tags
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List tags (should show "no tags" message)
        let cmd = List::new();
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_with_tags() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create some tags
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
            repo.create_tag("v2.0.0", TagOptions::default()).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List tags
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

        // Initialize a repository and create a tag
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List tags with verbose output
        let cmd = List::new().with_verbose(true);
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_list_annotated_only() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create tags
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
            repo.create_tag("v2.0.0", TagOptions::default().message("Annotated")).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // List only annotated tags
        let cmd = List::new().with_annotated_only(true);
        let result = cmd.run();
        assert!(result.is_ok());
    }
}

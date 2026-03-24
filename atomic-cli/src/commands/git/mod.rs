//! Git interoperability commands for Atomic VCS.
//!
//! This module provides commands for importing Git repositories into Atomic,
//! preserving commit history, authorship, timestamps, and file operations.
//!
//! # Commands
//!
//! - `import` - Import a Git repository's history into Atomic
//!
//! # Usage
//!
//! ```text
//! atomic git <COMMAND>
//!
//! Commands:
//!   import    Import a Git repository into Atomic
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ## Basic Import
//!
//! ```text
//! # Import current Git repository into Atomic
//! $ cd my-git-project
//! $ atomic git import
//! Importing 42 commits from 'main'...
//! Created 42 changes
//! ```
//!
//! ## Import Specific Branch
//!
//! ```text
//! $ atomic git import --branch feature-auth
//! ```
//!
//! ## Import All Branches
//!
//! ```text
//! $ atomic git import --all-branches
//! ```
//!
//! ## Incremental Import
//!
//! ```text
//! # After adding new Git commits
//! $ atomic git import --incremental
//! ```

pub mod import;
pub mod parallel;

use clap::Subcommand;

pub use import::Import;

use crate::commands::Command;
use crate::error::CliResult;

/// Subcommands for Git interoperability.
#[derive(Subcommand, Debug)]
pub enum GitCommands {
    /// Import a Git repository into Atomic.
    ///
    /// Converts Git commit history into Atomic changes, preserving:
    /// - Author name and email
    /// - Commit message (subject and body)
    /// - Commit timestamp
    /// - File additions, modifications, deletions, and renames
    /// - Git commit SHA (stored in change metadata)
    ///
    /// # Examples
    ///
    /// ```text
    /// # Import from current branch
    /// atomic git import
    ///
    /// # Import specific branch
    /// atomic git import --branch main
    ///
    /// # Import all branches as stacks
    /// atomic git import --all-branches
    ///
    /// # Preview without creating repository
    /// atomic git import --dry-run
    /// ```
    Import(Import),
}

/// Git interoperability command.
///
/// This is the top-level command that dispatches to Git subcommands.
#[derive(Debug, clap::Args)]
pub struct Git {
    #[command(subcommand)]
    pub command: GitCommands,
}

impl Command for Git {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            GitCommands::Import(cmd) => cmd.run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_commands_variants() {
        fn check_variant(cmd: &GitCommands) -> &'static str {
            match cmd {
                GitCommands::Import(_) => "import",
            }
        }

        let import = Import::default();
        assert_eq!(check_variant(&GitCommands::Import(import)), "import");
    }
}

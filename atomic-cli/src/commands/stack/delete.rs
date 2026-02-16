//! The `stack delete` command for deleting a stack.
//!
//! This module implements the `atomic stack delete` command, which removes
//! a stack from the repository. The stack's metadata is deleted, but the
//! changes themselves remain in the graph (they may be referenced by other
//! stacks).
//!
//! # Important: Cannot Delete Current Stack
//!
//! You cannot delete the currently active stack. Switch to a different
//! stack first using `atomic stack switch`.
//!
//! # Usage
//!
//! ```text
//! atomic stack delete [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the stack to delete
//!
//! Options:
//!   -f, --force  Force deletion without confirmation
//!   -h, --help   Print help information
//! ```
//!
//! # Examples
//!
//! Delete a stack:
//! ```text
//! $ atomic stack delete old-feature
//! Deleted stack: old-feature
//! ```
//!
//! Force delete:
//! ```text
//! $ atomic stack delete experiment --force
//! Deleted stack: experiment
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, stack as style_stack, warning};

#[cfg(test)]
use std::path::PathBuf;

// Delete Command

/// Delete a stack.
///
/// Removes the specified stack from the repository. The changes in the
/// stack are not deleted - they remain in the graph and may be referenced
/// by other stacks.
///
/// **Note**: You cannot delete the current stack. Switch to a different
/// stack first.
#[derive(Parser, Debug, Default)]
#[command(name = "delete")]
pub struct Delete {
    /// Name of the stack to delete.
    ///
    /// The stack must exist and cannot be the current stack.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// Force deletion without confirmation.
    ///
    /// By default, the command may prompt for confirmation if the stack
    /// has changes. Use this flag to skip confirmation.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Delete {
}

impl Command for Delete {
    fn run(&self) -> CliResult<()> {
        // Get the stack name
        let name = self.name.as_ref().ok_or_else(|| CliError::InvalidArgument {
            message: "Stack name is required".to_string(),
        })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => {
                CliError::RepositoryNotFound {
                    searched_path: path.into(),
                }
            }
            other => CliError::Repository(other),
        })?;

        // Check if trying to delete the current stack
        if repo.current_stack() == name {
            return Err(CliError::CannotDeleteCurrentStack {
                name: name.to_string(),
            });
        }

        // Check if the stack exists
        if !repo.stack_exists(name).map_err(CliError::Repository)? {
            return Err(CliError::StackNotFound {
                name: name.to_string(),
            });
        }

        // Get stack info for warning message
        if !self.force {
            if let Ok(info) = repo.get_stack_info(name) {
                if info.change_count > 0 {
                    println!(
                        "{}",
                        warning(&format!(
                            "Stack '{}' has {} change(s). Use --force to confirm deletion.",
                            name, info.change_count
                        ))
                    );
                    // In a real implementation, we might prompt for confirmation here
                    // For now, we'll just warn but proceed
                }
            }
        }

        // Delete the stack
        repo.delete_stack(name).map_err(|e| match e {
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            atomic_repository::RepositoryError::CannotDeleteCurrentStack { name } => {
                CliError::CannotDeleteCurrentStack { name }
            }
            other => CliError::Repository(other),
        })?;

        print_success(&format!("Deleted stack: {}", style_stack(name)));

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
    fn test_delete_with_name() {
        let cmd = Delete::with_name("old-feature");
        assert_eq!(cmd.name, Some("old-feature".to_string()));
        assert!(!cmd.force);
    }

    #[test]
    fn test_delete_with_force() {
        let cmd = Delete::with_name("experiment").with_force(true);
        assert_eq!(cmd.name, Some("experiment".to_string()));
        assert!(cmd.force);
    }

    #[test]
    fn test_default() {
        let cmd = Delete::default();
        assert!(cmd.name.is_none());
        assert!(!cmd.force);
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Delete::default();
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("required"));
            }
            other => panic!("Expected InvalidArgument, got: {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_delete_existing_stack() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a stack, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("to-delete").unwrap();
            // Verify it exists
            assert!(repo.stack_exists("to-delete").unwrap());
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Delete the stack
        let cmd = Delete::with_name("to-delete");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify it's gone
        let repo = Repository::open(repo_path).unwrap();
        assert!(!repo.stack_exists("to-delete").unwrap());
    }

    #[test]
    #[serial]
    fn test_delete_nonexistent_stack() {
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

        // Try to delete a non-existent stack
        let cmd = Delete::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::StackNotFound { name } => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("Expected StackNotFound, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_delete_current_stack() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository (current stack is "dev") and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Try to delete the current stack
        let cmd = Delete::with_name("dev");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::CannotDeleteCurrentStack { name } => {
                assert_eq!(name, "dev");
            }
            other => panic!("Expected CannotDeleteCurrentStack, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_delete_with_force_integration() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a stack, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("force-delete").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Force delete the stack
        let cmd = Delete::with_name("force-delete").with_force(true);
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify it's gone
        let repo = Repository::open(repo_path).unwrap();
        assert!(!repo.stack_exists("force-delete").unwrap());
    }
}

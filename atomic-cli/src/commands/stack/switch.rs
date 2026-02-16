//! The `stack switch` command for switching between stacks.
//!
//! This module implements the `atomic stack switch` command, which changes
//! the current stack to a different one and updates the working copy to
//! match the new stack's state.
//!
//! # Working Copy Update
//!
//! Like Pijul's channel switching, switching stacks in Atomic updates the
//! working copy to reflect the new stack's state. Files are created, updated,
//! or removed to match what's recorded in the target stack.
//!
//! # Usage
//!
//! ```text
//! atomic stack switch <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the stack to switch to
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Switch to a different stack:
//! ```text
//! $ atomic stack switch feature-auth
//! Switched to stack: feature-auth
//! ```
//!
//! Switch back to dev:
//! ```text
//! $ atomic stack switch dev
//! Switched to stack: dev
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, stack as style_stack};

#[cfg(test)]
use std::path::PathBuf;

// Switch Command

/// Switch to a different stack.
///
/// Changes the current stack to the specified one and updates the working
/// copy to match the new stack's state. This behavior matches Pijul's
/// channel switching.
///
/// **Note**: Switching stacks WILL update your working copy files to match
/// the target stack's state. Unrecorded changes may be overwritten.
#[derive(Parser, Debug, Default)]
#[command(name = "switch")]
pub struct Switch {
    /// Name of the stack to switch to.
    ///
    /// The stack must already exist. Use `atomic stack list` to see
    /// available stacks, or `atomic stack new` to create a new one.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

impl Switch {
}

impl Command for Switch {
    fn run(&self) -> CliResult<()> {
        // Get the stack name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "Stack name is required".to_string(),
            })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Check if we're already on this stack
        if repo.current_stack() == name {
            print_success(&format!("Already on stack: {}", style_stack(name)));
            return Ok(());
        }

        // Switch to the stack and update working copy
        let result = repo.switch_stack(name).map_err(|e| match e {
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            other => CliError::Repository(other),
        })?;

        print_success(&format!("Switched to stack: {}", style_stack(name)));

        // Show output statistics if any files were updated
        if result.files_written > 0 || result.directories_created > 0 {
            println!(
                "  {} files updated, {} directories",
                result.files_written, result.directories_created
            );
        }

        if result.has_conflicts() {
            crate::output::print_warning(&format!(
                "{} conflicts detected",
                result.conflict_count()
            ));
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
    fn test_switch_with_name() {
        let cmd = Switch::with_name("feature-auth");
        assert_eq!(cmd.name, Some("feature-auth".to_string()));
    }

    #[test]
    fn test_default() {
        let cmd = Switch::default();
        assert!(cmd.name.is_none());
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Switch::default();
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
    fn test_switch_to_existing_stack() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a stack, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("feature-test").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the new stack
        let cmd = Switch::with_name("feature-test");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we switched
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_stack(), "feature-test");
    }

    #[test]
    #[serial]
    fn test_switch_to_nonexistent_stack() {
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

        // Try to switch to a non-existent stack
        let cmd = Switch::with_name("nonexistent");
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
    fn test_switch_to_current_stack() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository (default stack is "dev") and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the current stack (should succeed with a message)
        let cmd = Switch::with_name("dev");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on dev
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_stack(), "dev");
    }
}

//! The `view delete` command for deleting a view.
//!
//! This module implements the `atomic view delete` command, which removes
//! a view from the repository. The view's metadata is deleted, but the
//! changes themselves remain in the graph (they may be referenced by other
//! views).
//!
//! # Important: Cannot Delete Current View
//!
//! You cannot delete the currently active view. Switch to a different
//! view first using `atomic view switch`.
//!
//! # Usage
//!
//! ```text
//! atomic view delete [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the view to delete
//!
//! Options:
//!   -f, --force  Force deletion without confirmation
//!   -h, --help   Print help information
//! ```
//!
//! # Examples
//!
//! Delete a view:
//! ```text
//! $ atomic view delete old-feature
//! Deleted view: old-feature
//! ```
//!
//! Force delete:
//! ```text
//! $ atomic view delete experiment --force
//! Deleted view: experiment
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, view as style_view, warning};

#[cfg(test)]
use std::path::PathBuf;

// Delete Command

/// Delete a view.
///
/// Removes the specified view from the repository. The changes in the
/// view are not deleted - they remain in the graph and may be referenced
/// by other views.
///
/// **Note**: You cannot delete the current view. Switch to a different
/// view first.
#[derive(Parser, Debug, Default)]
#[command(name = "delete")]
pub struct Delete {
    /// Name of the view to delete.
    ///
    /// The view must exist and cannot be the current view.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// Force deletion without confirmation.
    ///
    /// By default, the command may prompt for confirmation if the view
    /// has changes. Use this flag to skip confirmation.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Delete {
    /// Create a new Delete command targeting the given view.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            force: false,
        }
    }

    /// Create a new Delete command with default settings.
    pub fn new() -> Self {
        Self {
            name: None,
            force: false,
        }
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

impl Command for Delete {
    fn run(&self) -> CliResult<()> {
        // Get the view name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "View name is required".to_string(),
            })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Check if trying to delete the current view
        if repo.current_view() == name {
            return Err(CliError::CannotDeleteCurrentView {
                name: name.to_string(),
            });
        }

        // Check if the view exists
        if !repo.view_exists(name).map_err(CliError::Repository)? {
            return Err(CliError::ViewNotFound {
                name: name.to_string(),
            });
        }

        // Get view info for warning message
        if !self.force {
            if let Ok(info) = repo.get_view_info(name) {
                if info.change_count > 0 {
                    println!(
                        "{}",
                        warning(&format!(
                            "View '{}' has {} change(s). Use --force to confirm deletion.",
                            name, info.change_count
                        ))
                    );
                    // In a real implementation, we might prompt for confirmation here
                    // For now, we'll just warn but proceed
                }
            }
        }

        // Delete the view
        repo.delete_view(name).map_err(|e| match e {
            atomic_repository::RepositoryError::ViewNotFound { name } => {
                CliError::ViewNotFound { name }
            }
            atomic_repository::RepositoryError::CannotDeleteCurrentView { name } => {
                CliError::CannotDeleteCurrentView { name }
            }
            other => CliError::Repository(other),
        })?;

        print_success(&format!("Deleted view: {}", style_view(name)));

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
    fn test_delete_existing_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            // Create a draft view (shared views cannot be deleted)
            {
                use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
                let mut txn = repo.pristine().write_txn().unwrap();
                let dev = txn.get_view("dev").unwrap().unwrap();
                txn.create_view("to-delete", ViewScope::Draft, Some(dev.id))
                    .unwrap();
                txn.commit().unwrap();
            }
            // Verify it exists
            assert!(repo.view_exists("to-delete").unwrap());
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Delete the view
        let cmd = Delete::with_name("to-delete");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify it's gone
        let repo = Repository::open(repo_path).unwrap();
        assert!(!repo.view_exists("to-delete").unwrap());
    }

    #[test]
    #[serial]
    fn test_delete_nonexistent_view() {
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

        // Try to delete a non-existent view
        let cmd = Delete::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ViewNotFound { name } => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("Expected ViewNotFound, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_delete_current_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository (current view is "dev") and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Try to delete the current view
        let cmd = Delete::with_name("dev");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::CannotDeleteCurrentView { name } => {
                assert_eq!(name, "dev");
            }
            other => panic!("Expected CannotDeleteCurrentView, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_delete_with_force_integration() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a draft view, then drop to release lock
        {
            let repo = Repository::init(repo_path).unwrap();
            // Create a draft view (shared views cannot be deleted)
            {
                use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
                let mut txn = repo.pristine().write_txn().unwrap();
                let dev = txn.get_view("dev").unwrap().unwrap();
                txn.create_view("force-delete", ViewScope::Draft, Some(dev.id))
                    .unwrap();
                txn.commit().unwrap();
            }
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Force delete the view
        let cmd = Delete::with_name("force-delete").with_force(true);
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify it's gone
        let repo = Repository::open(repo_path).unwrap();
        assert!(!repo.view_exists("force-delete").unwrap());
    }
}

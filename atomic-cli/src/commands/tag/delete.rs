//! The `tag delete` command for deleting a tag.
//!
//! This module implements the `atomic tag delete` command, which removes
//! a tag from the repository. Deleting a tag does not affect the underlying
//! changes or view state - it only removes the named reference.
//!
//! # Usage
//!
//! ```text
//! atomic tag delete <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the tag to delete
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Delete a tag:
//! ```text
//! $ atomic tag delete v1.0.0
//! Deleted tag: v1.0.0
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint, print_success};

#[cfg(test)]
use std::path::PathBuf;

// Delete Command

/// Delete a tag.
///
/// Removes a tag from the repository. This does not affect the underlying
/// changes or view state - it only removes the named reference.
#[derive(Parser, Debug, Default)]
#[command(name = "delete")]
pub struct Delete {
    /// Name of the tag to delete.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

impl Delete {
    /// Create a new Delete command targeting the given tag.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
        }
    }
}

impl Command for Delete {
    fn run(&self) -> CliResult<()> {
        // Get the tag name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "Tag name is required".to_string(),
            })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Delete the tag
        let deleted = repo.delete_tag(name).map_err(CliError::Repository)?;

        if deleted {
            print_success(&format!("Deleted tag: {}", emphasis(name)));
        } else {
            println!("{}", hint(&format!("Tag '{}' does not exist", name)));
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
    fn test_delete_with_name() {
        let cmd = Delete::with_name("v1.0.0");
        assert_eq!(cmd.name, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_default() {
        let cmd = Delete::default();
        assert!(cmd.name.is_none());
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
    fn test_delete_existing_tag() {
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

        // Delete the tag
        let cmd = Delete::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the tag is gone
        let repo = Repository::open(repo_path).unwrap();
        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_none());
    }

    #[test]
    #[serial]
    fn test_delete_nonexistent_tag() {
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

        // Try to delete a non-existent tag (should succeed but report not found)
        let cmd = Delete::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_delete_preserves_other_tags() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create multiple tags
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
            repo.create_tag("v2.0.0", TagOptions::default()).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Delete only one tag
        let cmd = Delete::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the other tag still exists
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.get_tag("v1.0.0").unwrap().is_none());
        assert!(repo.get_tag("v2.0.0").unwrap().is_some());
    }
}

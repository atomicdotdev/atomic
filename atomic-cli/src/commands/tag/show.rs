//! The `tag show` command for displaying tag details.
//!
//! This module implements the `atomic tag show` command, which displays
//! detailed information about a specific tag including its state, sequence
//! number, timestamp, and any annotation (message/author).
//!
//! # Usage
//!
//! ```text
//! atomic tag show <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the tag to show
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Show tag details:
//! ```text
//! $ atomic tag show v1.0.0
//! Tag: v1.0.0
//! View: dev
//! Sequence: 42
//! State: ABCDEF123456789...
//! Created: 2024-01-15 10:30:00 UTC
//! Type: annotated
//! Message: Release version 1.0.0
//! Author: Alice <alice@example.com>
//! ```

use clap::Parser;

use atomic_core::types::Base32;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, hint};

#[cfg(test)]
use std::path::PathBuf;

// Show Command

/// Show details for a specific tag.
///
/// Displays detailed information about a tag including its state,
/// sequence number, timestamp, and any annotation.
#[derive(Parser, Debug, Default)]
#[command(name = "show")]
pub struct Show {
    /// Name of the tag to show.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

impl Show {
    /// Create a new Show command targeting the given tag.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
        }
    }
}

impl Command for Show {
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

        // Get the tag
        let tag = repo.get_tag(name).map_err(CliError::Repository)?;

        match tag {
            Some(tag) => {
                println!("{}: {}", emphasis("Tag"), tag.name);
                println!("{}: {}", emphasis("View"), tag.view);
                println!("{}: {}", emphasis("Sequence"), tag.sequence);
                println!("{}: {}", emphasis("State"), tag.state.to_base32());
                println!(
                    "{}: {}",
                    emphasis("Created"),
                    tag.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
                );
                println!("{}: {}", emphasis("Kind"), tag.kind);
                println!(
                    "{}: {}",
                    emphasis("Type"),
                    if tag.is_annotated() {
                        "annotated"
                    } else {
                        "lightweight"
                    }
                );

                if let Some(ref message) = tag.message {
                    println!("{}: {}", emphasis("Message"), message);
                }

                if let Some(ref author) = tag.author {
                    let author_str = match &author.email {
                        Some(email) => format!("{} <{}>", author.name, email),
                        None => author.name.clone(),
                    };
                    println!("{}: {}", emphasis("Author"), author_str);
                }

                if let Some(ref metadata) = tag.metadata {
                    println!("{}: {}", emphasis("Metadata"), metadata);
                }

                Ok(())
            }
            None => {
                println!(
                    "{}",
                    hint(&format!(
                        "Tag '{}' not found. Use 'atomic tag list' to see available tags.",
                        name
                    ))
                );
                Ok(())
            }
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::TagKind;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_show_with_name() {
        let cmd = Show::with_name("v1.0.0");
        assert_eq!(cmd.name, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_default() {
        let cmd = Show::default();
        assert!(cmd.name.is_none());
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Show::default();
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
    fn test_show_existing_tag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a tag
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", None, TagKind::Release).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Show the tag
        let cmd = Show::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_show_annotated_tag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create an annotated tag
        {
            let repo = Repository::init(repo_path).unwrap();
            repo.create_tag("v1.0.0", Some("Release version 1.0.0"), TagKind::Release)
                .unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Show the tag
        let cmd = Show::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_show_nonexistent_tag() {
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

        // Try to show a non-existent tag (should succeed with "not found" message)
        let cmd = Show::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_ok());
    }
}

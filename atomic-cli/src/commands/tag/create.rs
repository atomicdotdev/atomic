//! The `tag create` command for creating a new tag.
//!
//! This module implements the `atomic tag create` command, which creates a new
//! tag pointing to the current state of a stack. Tags can be lightweight
//! (just a reference) or annotated (with message and author information).
//!
//! # Usage
//!
//! ```text
//! atomic tag create [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the tag to create
//!
//! Options:
//!   -m, --message <MSG>   Message for an annotated tag
//!   -a, --author <AUTHOR> Author for an annotated tag (format: "Name <email>")
//!   -s, --stack <STACK>   Stack to tag (default: current stack)
//!   -f, --force           Overwrite existing tag
//!   -h, --help            Print help information
//! ```
//!
//! # Examples
//!
//! Create a lightweight tag:
//! ```text
//! $ atomic tag create v1.0.0
//! Created tag: v1.0.0
//! ```
//!
//! Create an annotated tag with a message:
//! ```text
//! $ atomic tag create v1.0.0 -m "Release version 1.0.0"
//! Created annotated tag: v1.0.0
//! ```

use clap::Parser;

use atomic_repository::{Repository, TagOptions};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{emphasis, print_success};

#[cfg(test)]
use std::path::PathBuf;

// Author Parsing

/// Parse an author string in the format "Name <email>" or just "Name".
///
/// # Arguments
///
/// * `s` - The author string to parse
///
/// # Returns
///
/// A tuple of (name, optional_email).
fn parse_author(s: &str) -> (String, Option<String>) {
    let s = s.trim();

    // Try to match "Name <email>" format
    if let Some(start) = s.find('<') {
        if let Some(end) = s.find('>') {
            if start < end {
                let name = s[..start].trim().to_string();
                let email = s[start + 1..end].trim().to_string();
                return (name, Some(email));
            }
        }
    }

    // Just a name
    (s.to_string(), None)
}

// Create Command

/// Create a new tag.
///
/// Creates a tag pointing to the current state of a stack. Tags can be
/// lightweight (just a reference) or annotated (with message and author).
#[derive(Parser, Debug, Default)]
#[command(name = "create")]
pub struct Create {
    /// Name of the tag to create.
    ///
    /// Tag names should follow a naming convention like `v1.0.0`,
    /// `release-2024-01`, etc.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// Message for an annotated tag.
    ///
    /// If provided, creates an annotated tag with this message.
    #[arg(long, short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Author for an annotated tag.
    ///
    /// Format: "Name <email>" or just "Name".
    /// If not provided, uses the default identity.
    #[arg(long, short = 'a', value_name = "AUTHOR")]
    pub author: Option<String>,

    /// Stack to tag.
    ///
    /// If not provided, uses the current stack.
    #[arg(long, short = 's', value_name = "STACK")]
    pub stack: Option<String>,

    /// Overwrite existing tag.
    ///
    /// If a tag with this name already exists, overwrite it.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Create {
    /// Create a new Create command with default settings.
    pub fn new() -> Self {
        Self {
            name: None,
            message: None,
            author: None,
            stack: None,
            force: false,
        }
    }

    /// Create a new Create command with the given tag name.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            message: None,
            author: None,
            stack: None,
            force: false,
        }
    }

    /// Builder: set the message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Builder: set the author.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Builder: set the stack.
    pub fn with_stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }
}

impl Command for Create {
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

        // Build tag options
        let mut options = TagOptions::new();

        if let Some(msg) = &self.message {
            options = options.message(msg);
        }

        if let Some(author_str) = &self.author {
            let (name, email) = parse_author(author_str);
            options = options.author(name, email);
        }

        if let Some(stack) = &self.stack {
            options = options.stack(stack);
        }

        if self.force {
            options = options.force(true);
        }

        // Create the tag
        let tag = repo.create_tag(name, options).map_err(|e| match e {
            atomic_repository::RepositoryError::TagAlreadyExists { name } => {
                CliError::InvalidArgument {
                    message: format!("Tag '{}' already exists. Use --force to overwrite.", name),
                }
            }
            atomic_repository::RepositoryError::InvalidTagName { name, reason } => {
                CliError::InvalidArgument {
                    message: format!("Invalid tag name '{}': {}", name, reason),
                }
            }
            atomic_repository::RepositoryError::StackNotFound { name } => {
                CliError::StackNotFound { name }
            }
            other => CliError::Repository(other),
        })?;

        // Print success message
        if tag.is_annotated() {
            print_success(&format!("Created annotated tag: {}", emphasis(&tag.name)));
        } else {
            print_success(&format!("Created tag: {}", emphasis(&tag.name)));
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
    // Author Parsing Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_author_with_email() {
        let (name, email) = parse_author("Alice <alice@example.com>");
        assert_eq!(name, "Alice");
        assert_eq!(email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_author_without_email() {
        let (name, email) = parse_author("Bob");
        assert_eq!(name, "Bob");
        assert_eq!(email, None);
    }

    #[test]
    fn test_parse_author_with_spaces() {
        let (name, email) = parse_author("  Alice Smith  <alice@example.com>  ");
        assert_eq!(name, "Alice Smith");
        assert_eq!(email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_author_empty_email() {
        let (name, email) = parse_author("Alice <>");
        assert_eq!(name, "Alice");
        assert_eq!(email, Some("".to_string()));
    }

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_create_with_name() {
        let cmd = Create::with_name("v1.0.0");
        assert_eq!(cmd.name, Some("v1.0.0".to_string()));
        assert!(cmd.message.is_none());
        assert!(cmd.author.is_none());
        assert!(!cmd.force);
    }

    #[test]
    fn test_create_with_message() {
        let cmd = Create::with_name("v1.0.0").with_message("Release 1.0");
        assert_eq!(cmd.name, Some("v1.0.0".to_string()));
        assert_eq!(cmd.message, Some("Release 1.0".to_string()));
    }

    #[test]
    fn test_create_with_author() {
        let cmd = Create::with_name("v1.0.0").with_author("Alice <alice@example.com>");
        assert_eq!(cmd.author, Some("Alice <alice@example.com>".to_string()));
    }

    #[test]
    fn test_create_with_stack() {
        let cmd = Create::with_name("v1.0.0").with_stack("release");
        assert_eq!(cmd.stack, Some("release".to_string()));
    }

    #[test]
    fn test_create_with_force() {
        let cmd = Create::with_name("v1.0.0").with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_default() {
        let cmd = Create::default();
        assert!(cmd.name.is_none());
        assert!(cmd.message.is_none());
        assert!(cmd.author.is_none());
        assert!(cmd.stack.is_none());
        assert!(!cmd.force);
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Create::default();
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
    fn test_create_lightweight_tag() {
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

        let cmd = Create::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the tag exists
        let repo = Repository::open(repo_path).unwrap();
        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_some());
        assert!(!tag.unwrap().is_annotated());
    }

    #[test]
    #[serial]
    fn test_create_annotated_tag() {
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

        let cmd = Create::with_name("v1.0.0")
            .with_message("Release version 1.0.0")
            .with_author("Alice <alice@example.com>");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the tag is annotated
        let repo = Repository::open(repo_path).unwrap();
        let tag = repo.get_tag("v1.0.0").unwrap().unwrap();
        assert!(tag.is_annotated());
        assert_eq!(tag.message(), Some("Release version 1.0.0"));
    }

    #[test]
    #[serial]
    fn test_create_duplicate_tag_fails() {
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

        // Try to create the same tag again
        let cmd = Create::with_name("v1.0.0");
        let result = cmd.run();
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_create_with_force_overwrites() {
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

        // Create with force should succeed
        let cmd = Create::with_name("v1.0.0").with_force(true);
        let result = cmd.run();
        assert!(result.is_ok());
    }
}

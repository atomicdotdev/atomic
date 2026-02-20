//! The `stack new` command for creating new stacks.
//!
//! # Two-Tier Stack Model
//!
//! Stacks can be **Shared** (default) or **Local**:
//!
//! - **Shared** stacks (dev, release, main) write edges to the global graph.
//!   They are permanent and visible to all stacks.
//! - **Local** stacks (feature, bug, experiment) write edges to a per-stack
//!   graph. They can be deleted cleanly with zero orphaned edges.
//!
//! Use `--local` to create an local workspace. Use `--parent` to set the
//! parent stack (defaults to the current stack).
//!
//! This module implements the `atomic stack new` command, which creates a new
//! stack in the repository. Stacks in Atomic are views of the graph - they
//! represent which changes have been applied and in what order.
//!
//! # Usage
//!
//! ```text
//! atomic stack new [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the new stack
//!
//! Options:
//!       --from <STACK>  Create from an existing stack (fork/split)
//!   -s, --switch        Switch to the new stack after creating it
//!   -h, --help          Print help information
//! ```
//!
//! # Examples
//!
//! Create a new empty stack:
//! ```text
//! $ atomic stack new feature-auth
//! Created stack: feature-auth
//! ```
//!
//! Create a stack from another (fork/split):
//! ```text
//! $ atomic stack new hotfix --from main
//! Created stack: hotfix (forked from main with 42 changes)
//! ```
//!
//! Create and switch to a new stack:
//! ```text
//! $ atomic stack new feature-auth --switch
//! Created stack: feature-auth
//! Switched to stack: feature-auth
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, stack as style_stack};

#[cfg(test)]
use std::path::PathBuf;

// Constants

/// Maximum length for a stack name.
const MAX_STACK_NAME_LENGTH: usize = 255;

/// Characters not allowed in stack names.
const INVALID_CHARS: &[char] = &['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|', ' '];

// Stack Name Validation

/// Validate a stack name.
///
/// Stack names must:
/// - Not be empty
/// - Not exceed 255 characters
/// - Not contain invalid characters (/, \, :, *, ?, ", <, >, |, space, null)
/// - Not start or end with a dot
/// - Not be "." or ".."
///
/// # Arguments
///
/// * `name` - The stack name to validate
///
/// # Returns
///
/// `Ok(())` if the name is valid, or an error describing why it's invalid.
fn validate_stack_name(name: &str) -> Result<(), String> {
    // Check for empty name
    if name.is_empty() {
        return Err("Stack name cannot be empty".to_string());
    }

    // Check length
    if name.len() > MAX_STACK_NAME_LENGTH {
        return Err(format!(
            "Stack name cannot exceed {} characters",
            MAX_STACK_NAME_LENGTH
        ));
    }

    // Check for invalid characters
    for c in INVALID_CHARS {
        if name.contains(*c) {
            let char_desc = match c {
                ' ' => "spaces".to_string(),
                '\0' => "null characters".to_string(),
                _ => format!("'{}'", c),
            };
            return Err(format!("Stack name cannot contain {}", char_desc));
        }
    }

    // Check for reserved names
    if name == "." || name == ".." {
        return Err("Stack name cannot be '.' or '..'".to_string());
    }

    // Check for leading/trailing dots
    if name.starts_with('.') {
        return Err("Stack name cannot start with a dot".to_string());
    }
    if name.ends_with('.') {
        return Err("Stack name cannot end with a dot".to_string());
    }

    Ok(())
}

// New Command

/// Create a new stack.
///
/// Creates a new stack in the repository. By default, the new stack starts
/// empty (with no changes applied). Use `--from` to fork from an existing
/// stack, copying all its changes to the new stack.
#[derive(Parser, Debug, Default)]
#[command(name = "new")]
pub struct New {
    /// Name of the new stack.
    ///
    /// Stack names should be descriptive and follow a naming convention
    /// like `feature-*`, `bugfix-*`, `release-*`, etc.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// Create from an existing stack (fork/split).
    ///
    /// When specified, the new stack will be created with all changes
    /// from the source stack applied. This is useful for creating
    /// feature branches or hotfix stacks.
    ///
    /// If not specified, defaults to forking from the current stack.
    /// Use `--empty` to create a stack with no history.
    #[arg(long, value_name = "STACK")]
    pub from: Option<String>,

    /// Create an empty stack with no history.
    ///
    /// By default, new stacks are forked from the current stack.
    /// Use this flag to create a truly independent stack with no
    /// changes applied. This is rarely needed and primarily useful
    /// for advanced workflows like importing external changes.
    #[arg(long)]
    pub empty: bool,

    /// Switch to the new stack after creating it.
    ///
    /// By default, the current stack remains unchanged after creating
    /// a new stack. Use this flag to automatically switch to the new stack.
    #[arg(long, short = 's')]
    pub switch: bool,

    /// Create an local workspace (ephemeral, deletable).
    ///
    /// Local workspaces write edges to a per-stack graph (`STACK_GRAPH`)
    /// instead of the global graph. When deleted, all their edges are
    /// cascade-removed with zero orphans.
    ///
    /// Without this flag, stacks are created as **shared** (permanent).
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a local feature stack parented on dev
    /// atomic stack new feature-auth --local
    ///
    /// # Create an local workspace with an explicit parent
    /// atomic stack new feature-login --local --parent service-auth
    /// ```
    #[arg(long, short = 'i')]
    pub local: bool,

    /// Parent stack for the new stack.
    ///
    /// Sets the parent in the stack hierarchy. The parent determines
    /// the overlay chain for graph traversal: an local workspace sees
    /// its own edges plus its parent's effective view (recursively).
    ///
    /// Defaults to the current stack. Use `--parent` to specify a
    /// different parent explicitly.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Parent on a long-lived service stack
    /// atomic stack new feature-login --local --parent service-auth
    ///
    /// # Parent on dev (the default if dev is current)
    /// atomic stack new bugfix-123 --local --parent dev
    /// ```
    #[arg(long, value_name = "STACK")]
    pub parent: Option<String>,
}

impl New {
    /// Create a new New command with the given stack name.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            from: None,
            empty: false,
            switch: false,
            local: false,
            parent: None,
        }
    }

    /// Builder: set the source stack to fork from.
    pub fn with_from(mut self, from: impl Into<String>) -> Self {
        self.from = Some(from.into());
        self
    }

    /// Builder: set the empty flag.
    pub fn with_empty(mut self, empty: bool) -> Self {
        self.empty = empty;
        self
    }

    /// Builder: set the switch flag.
    pub fn with_switch(mut self, switch: bool) -> Self {
        self.switch = switch;
        self
    }

    /// Two-tier stack creation: --local and/or --parent
    fn run_two_tier(&self, name: &str, repo: &mut Repository) -> CliResult<()> {
        use atomic_core::pristine::{MutTxnT, StackKind, StackTxnT};

        let kind = if self.local {
            StackKind::Local
        } else {
            StackKind::Shared
        };

        // Resolve the parent stack name → ID
        let parent_name = self
            .parent
            .clone()
            .unwrap_or_else(|| repo.current_stack().to_string());

        let mut txn = repo
            .pristine()
            .write_txn()
            .map_err(|e| CliError::Internal(e.into()))?;

        let parent_stack = txn
            .get_stack(&parent_name)
            .map_err(|e| CliError::Internal(e.into()))?
            .ok_or_else(|| CliError::StackNotFound {
                name: parent_name.clone(),
            })?;

        let parent_id = parent_stack.id;

        // Create the stack with explicit kind and parent
        let _stack = txn
            .create_stack(name, kind, Some(parent_id))
            .map_err(|e| CliError::Internal(e.into()))?;

        txn.commit().map_err(|e| CliError::Internal(e.into()))?;

        let kind_label = if kind.is_local() { "local" } else { "shared" };

        print_success(&format!(
            "Created {} stack: {} (parent: {})",
            kind_label,
            style_stack(name),
            style_stack(&parent_name),
        ));

        self.maybe_switch(name, repo)
    }

    /// Optionally switch to the new stack and print hint.
    fn maybe_switch(&self, name: &str, repo: &mut Repository) -> CliResult<()> {
        if self.switch {
            repo.set_current_stack(name).map_err(CliError::Repository)?;
            print_success(&format!("Switched to stack: {}", style_stack(name)));
        } else {
            print_hint(&format!(
                "Use 'atomic stack switch {}' to switch to the new stack",
                name
            ));
        }
        Ok(())
    }
}

impl Command for New {
    fn run(&self) -> CliResult<()> {
        // Get the stack name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "Stack name is required".to_string(),
            })?;

        // Validate the stack name
        validate_stack_name(name).map_err(|msg| CliError::InvalidArgument { message: msg })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Check if the stack already exists
        if repo.stack_exists(name).map_err(CliError::Repository)? {
            return Err(CliError::StackAlreadyExists {
                name: name.to_string(),
            });
        }

        // If --local or --parent is specified, use the two-tier create path
        if self.local || self.parent.is_some() {
            return self.run_two_tier(name, &mut repo);
        }

        // Legacy path: --empty or --from (backward compatible)
        // Determine the source stack:
        // 1. If --empty is specified, create an orphan stack with no history
        // 2. If --from is specified, use that stack
        // 3. Otherwise, default to forking from the current stack
        if self.empty {
            // Create an empty stack (rare use case)
            repo.create_stack(name).map_err(CliError::Repository)?;
            print_success(&format!("Created stack: {} (empty)", style_stack(name)));
        } else {
            // Determine source: explicit --from or current stack
            let source = self
                .from
                .clone()
                .unwrap_or_else(|| repo.current_stack().to_string());

            if !repo.stack_exists(&source).map_err(CliError::Repository)? {
                return Err(CliError::StackNotFound {
                    name: source.to_string(),
                });
            }

            // Get source stack info for reporting
            let source_info = repo.get_stack_info(&source).map_err(CliError::Repository)?;
            let change_count = source_info.change_count;

            // Create the stack by copying change log from source
            // This does NOT re-apply changes - it just copies metadata
            repo.create_stack_from(name, &source)
                .map_err(CliError::Repository)?;

            if change_count > 0 {
                print_success(&format!(
                    "Created stack: {} (forked from {} with {} changes)",
                    style_stack(name),
                    style_stack(&source),
                    change_count
                ));
            } else {
                print_success(&format!(
                    "Created stack: {} (forked from {} - empty)",
                    style_stack(name),
                    style_stack(&source)
                ));
            }
        }

        self.maybe_switch(name, &mut repo)
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
    // Stack Name Validation Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_valid_stack_names() {
        assert!(validate_stack_name("main").is_ok());
        assert!(validate_stack_name("dev").is_ok());
        assert!(validate_stack_name("feature-auth").is_ok());
        assert!(validate_stack_name("feature_auth").is_ok());
        assert!(validate_stack_name("bugfix-123").is_ok());
        assert!(validate_stack_name("release-1.0.0").is_ok());
        assert!(validate_stack_name("user@domain").is_ok());
        assert!(validate_stack_name("CamelCase").is_ok());
        assert!(validate_stack_name("UPPERCASE").is_ok());
        assert!(validate_stack_name("a").is_ok());
        assert!(validate_stack_name("123").is_ok());
    }

    #[test]
    fn test_empty_name() {
        let result = validate_stack_name("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(MAX_STACK_NAME_LENGTH + 1);
        let result = validate_stack_name(&long_name);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("255"));
    }

    #[test]
    fn test_invalid_characters() {
        assert!(validate_stack_name("feature/auth").is_err());
        assert!(validate_stack_name("feature\\auth").is_err());
        assert!(validate_stack_name("feature:auth").is_err());
        assert!(validate_stack_name("feature*auth").is_err());
        assert!(validate_stack_name("feature?auth").is_err());
        assert!(validate_stack_name("feature\"auth").is_err());
        assert!(validate_stack_name("feature<auth").is_err());
        assert!(validate_stack_name("feature>auth").is_err());
        assert!(validate_stack_name("feature|auth").is_err());
        assert!(validate_stack_name("feature auth").is_err());
    }

    #[test]
    fn test_reserved_names() {
        assert!(validate_stack_name(".").is_err());
        assert!(validate_stack_name("..").is_err());
    }

    #[test]
    fn test_dot_restrictions() {
        assert!(validate_stack_name(".hidden").is_err());
        assert!(validate_stack_name("trailing.").is_err());
        // Dots in the middle are allowed
        assert!(validate_stack_name("feature.auth").is_ok());
        assert!(validate_stack_name("v1.0.0").is_ok());
    }

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_new_with_name() {
        let cmd = New::with_name("feature-auth");
        assert_eq!(cmd.name, Some("feature-auth".to_string()));
        assert!(!cmd.switch);
        assert!(!cmd.empty);
    }

    #[test]
    fn test_new_with_switch() {
        let cmd = New::with_name("feature-auth").with_switch(true);
        assert_eq!(cmd.name, Some("feature-auth".to_string()));
        assert!(cmd.switch);
        assert!(!cmd.empty);
    }

    #[test]
    fn test_new_with_empty() {
        let cmd = New::with_name("orphan-stack").with_empty(true);
        assert_eq!(cmd.name, Some("orphan-stack".to_string()));
        assert!(cmd.empty);
        assert!(!cmd.switch);
    }

    #[test]
    fn test_default() {
        let cmd = New::default();
        assert!(cmd.name.is_none());
        assert!(!cmd.switch);
        assert!(!cmd.empty);
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = New::default();
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("required"));
            }
            other => panic!("Expected InvalidArgument, got: {:?}", other),
        }
    }

    #[test]
    fn test_run_with_invalid_name() {
        let cmd = New::with_name("invalid/name");
        let result = cmd.run();
        // Should fail with InvalidArgument before even trying to open repo
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("'/'"));
            }
            CliError::RepositoryNotFound { .. } => {
                // Also acceptable - validation passed but no repo
            }
            other => panic!(
                "Expected InvalidArgument or RepositoryNotFound, got: {:?}",
                other
            ),
        }
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_run_creates_stack_forked_from_current() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory and create a stack
        std::env::set_current_dir(repo_path).unwrap();

        // Create a stack without --from (should default to forking from current stack)
        let cmd = New::with_name("feature-test");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the stack exists and was forked from dev (the default stack)
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.stack_exists("feature-test").unwrap());

        // Both stacks should have the same state (since dev is empty, feature-test
        // should also be empty but forked from dev's changelog)
        let dev_info = repo.get_stack_info("dev").unwrap();
        let feature_info = repo.get_stack_info("feature-test").unwrap();
        assert_eq!(dev_info.change_count, feature_info.change_count);
    }

    #[test]
    #[serial]
    fn test_run_creates_empty_stack_with_flag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create an empty (orphan) stack with --empty flag
        let cmd = New::with_name("orphan-stack").with_empty(true);
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the stack exists
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.stack_exists("orphan-stack").unwrap());

        // The orphan stack should have 0 changes (truly empty, not forked)
        let orphan_info = repo.get_stack_info("orphan-stack").unwrap();
        assert_eq!(orphan_info.change_count, 0);
    }

    #[test]
    #[serial]
    fn test_run_with_switch() {
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

        let cmd = New::with_name("feature-switch").with_switch(true);
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we switched to the new stack
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_stack(), "feature-switch");
    }

    #[test]
    #[serial]
    fn test_run_duplicate_stack() {
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

        // Create the stack first time
        let cmd = New::with_name("duplicate");
        assert!(cmd.run().is_ok());

        // Try to create it again
        let cmd = New::with_name("duplicate");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::StackAlreadyExists { name } => {
                assert_eq!(name, "duplicate");
            }
            other => panic!("Expected StackAlreadyExists, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_run_with_explicit_from() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a source stack
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_stack("source-stack").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create a stack explicitly forked from source-stack
        let cmd = New::with_name("forked-stack").with_from("source-stack");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the stack exists
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.stack_exists("forked-stack").unwrap());
    }

    #[test]
    #[serial]
    fn test_run_from_nonexistent_stack_fails() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Try to create a stack from a nonexistent source
        let cmd = New::with_name("new-stack").with_from("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::StackNotFound { name } => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("Expected StackNotFound, got: {:?}", other),
        }
    }
}

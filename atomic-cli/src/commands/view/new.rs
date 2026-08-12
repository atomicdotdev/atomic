//! The `view create` command for creating new views.
//!
//! # Two-Tier View Model
//!
//! Views can be **Shared** (default) or **Draft**:
//!
//! - **Shared** views (dev, release, main) write edges to the global graph.
//!   They are permanent and visible to all views.
//! - **Draft** views (feature, bug, experiment) write edges to a per-view
//!   graph. They can be deleted cleanly with zero orphaned edges.
//!
//! Use `--draft` to create a draft workspace. Use `--parent` to set the
//! parent view (defaults to the current view).
//!
//! This module implements the `atomic view create` command, which creates a new
//! view in the repository. Views in Atomic are perspectives on the graph - they
//! represent which changes have been inserted and in what order.
//!
//! # Usage
//!
//! ```text
//! atomic view create [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the new view
//!
//! Options:
//!       --from <VIEW>   Create from an existing view (fork/split)
//!   -s, --switch        Switch to the new view after creating it
//!   -h, --help          Print help information
//! ```
//!
//! # Examples
//!
//! Create a new empty view:
//! ```text
//! $ atomic view create feature-auth
//! Created view: feature-auth
//! ```
//!
//! Create a view from another (fork/split):
//! ```text
//! $ atomic view create hotfix --from main
//! Created view: hotfix (forked from main with 42 changes)
//! ```
//!
//! Create and switch to a new view:
//! ```text
//! $ atomic view create feature-auth --switch
//! Created view: feature-auth
//! Switched to view: feature-auth
//! ```

use clap::Parser;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::complete::complete_view_names;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, view as style_view};

#[cfg(test)]
use std::path::PathBuf;

// Constants

/// Maximum length for a view name.
const MAX_VIEW_NAME_LENGTH: usize = 255;

/// Characters not allowed in view names.
const INVALID_CHARS: &[char] = &['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|', ' '];

// View Name Validation

/// Validate a view name.
///
/// View names must:
/// - Not be empty
/// - Not exceed 255 characters
/// - Not contain invalid characters (/, \, :, *, ?, ", <, >, |, space, null)
/// - Not start or end with a dot
/// - Not be "." or ".."
///
/// # Arguments
///
/// * `name` - The view name to validate
///
/// # Returns
///
/// `Ok(())` if the name is valid, or an error describing why it's invalid.
fn validate_view_name(name: &str) -> Result<(), String> {
    // Check for empty name
    if name.is_empty() {
        return Err("View name cannot be empty".to_string());
    }

    // Check length
    if name.len() > MAX_VIEW_NAME_LENGTH {
        return Err(format!(
            "View name cannot exceed {} characters",
            MAX_VIEW_NAME_LENGTH
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
            return Err(format!("View name cannot contain {}", char_desc));
        }
    }

    // Check for reserved names
    if name == "." || name == ".." {
        return Err("View name cannot be '.' or '..'".to_string());
    }

    // Check for leading/trailing dots
    if name.starts_with('.') {
        return Err("View name cannot start with a dot".to_string());
    }
    if name.ends_with('.') {
        return Err("View name cannot end with a dot".to_string());
    }

    Ok(())
}

// New Command

/// Create a new view.
///
/// Creates a new view in the repository. By default, the new view starts
/// empty (with no changes inserted). Use `--from` to fork from an existing
/// view, copying all its changes to the new view.
#[derive(Parser, Debug, Default)]
#[command(name = "create")]
pub struct New {
    /// Name of the new view.
    ///
    /// View names should be descriptive and follow a naming convention
    /// like `feature-*`, `bugfix-*`, `release-*`, etc.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,

    /// Fork from a specific view instead of the current one.
    ///
    /// By default, `view create` forks from the current view.  Use
    /// `--from <VIEW>` to fork from a different view instead.
    ///
    /// The new view inherits all changes from the source and gets
    /// its own view filter on the canonical `GRAPH` so that future
    /// changes recorded on it are invisible to the source.
    #[arg(long, value_name = "VIEW", add = ArgValueCompleter::new(complete_view_names))]
    pub from: Option<String>,

    /// Create an empty view with no inherited history.
    ///
    /// Rarely needed — this is what `stash` is for.  Kept for
    /// backward compatibility and advanced workflows like importing
    /// external changes.
    #[arg(long, hide = true)]
    pub empty: bool,

    /// Switch to the new view after creating it.
    ///
    /// By default, the current view remains unchanged after creating
    /// a new view. Use this flag to automatically switch to the new view.
    #[arg(long, short = 's')]
    pub switch: bool,

    /// Create a draft workspace (ephemeral, deletable).
    ///
    /// Draft workspaces write edges to the canonical `GRAPH` (filtered by view)
    /// instead of the global graph. When deleted, all their edges are
    /// cascade-removed with zero orphans.
    ///
    /// Without this flag, views are created as **shared** (permanent).
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a draft feature view parented on dev
    /// atomic view create feature-auth --draft
    ///
    /// # Create a draft workspace with an explicit parent
    /// atomic view create feature-login --draft --parent service-auth
    /// ```
    #[arg(long, short = 'd')]
    pub draft: bool,

    /// Parent view for the new view.
    ///
    /// Sets the parent in the view hierarchy. The parent determines
    /// the overlay chain for graph traversal: a draft workspace sees
    /// its own edges plus its parent's effective view (recursively).
    ///
    /// Defaults to the current view. Use `--parent` to specify a
    /// different parent explicitly.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Parent on a long-lived service view
    /// atomic view create feature-login --draft --parent service-auth
    ///
    /// # Parent on dev (the default if dev is current)
    /// atomic view create bugfix-123 --draft --parent dev
    /// ```
    #[arg(long, value_name = "VIEW", add = ArgValueCompleter::new(complete_view_names))]
    pub parent: Option<String>,
}

impl New {
    /// Create a new New command with the given view name.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            from: None,
            empty: false,
            switch: false,
            draft: false,
            parent: None,
        }
    }

    /// Builder: set the source view to fork from.
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

    /// Two-tier view creation: --draft and/or --parent
    fn run_two_tier(&self, name: &str, repo: &mut Repository) -> CliResult<()> {
        use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};

        let kind = if self.draft {
            ViewScope::Draft
        } else {
            ViewScope::Shared
        };

        // Resolve the parent view name → ID
        let parent_name = self
            .parent
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string());

        let mut txn = repo
            .pristine()
            .write_txn()
            .map_err(|e| CliError::Internal(e.into()))?;

        let parent_view = txn
            .get_view(&parent_name)
            .map_err(|e| CliError::Internal(e.into()))?
            .ok_or_else(|| CliError::ViewNotFound {
                name: parent_name.clone(),
            })?;

        let parent_id = parent_view.id;

        // Create the view with explicit kind and parent
        let _view = txn
            .create_view(name, kind, Some(parent_id))
            .map_err(|e| CliError::Internal(e.into()))?;

        txn.commit().map_err(|e| CliError::Internal(e.into()))?;

        let kind_label = if kind.is_draft() { "draft" } else { "shared" };

        print_success(&format!(
            "Created {} view: {} (parent: {})",
            kind_label,
            style_view(name),
            style_view(&parent_name),
        ));

        self.maybe_switch(name, repo)
    }

    /// Optionally switch to the new view and print hint.
    fn maybe_switch(&self, name: &str, repo: &mut Repository) -> CliResult<()> {
        if self.switch {
            let result = repo.switch_view(name).map_err(CliError::Repository)?;
            print_success(&format!(
                "Switched to view: {} ({} files updated)",
                style_view(name),
                result.files_written,
            ));
        } else {
            print_hint(&format!(
                "Use 'atomic view switch {}' to switch to the new view",
                name
            ));
        }
        Ok(())
    }
}

impl Command for New {
    fn run(&self) -> CliResult<()> {
        // Get the view name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "View name is required".to_string(),
            })?;

        // Validate the view name
        validate_view_name(name).map_err(|msg| CliError::InvalidArgument { message: msg })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Check if the view already exists
        if repo.view_exists(name).map_err(CliError::Repository)? {
            return Err(CliError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        // If --draft or --parent is specified, use the two-tier create path
        if self.draft || self.parent.is_some() {
            return self.run_two_tier(name, &mut repo);
        }

        // Determine how to create the new view:
        //
        //   --from X → create Draft parented on X, insert X's changes
        //   default  → create Draft parented on nearest Shared ancestor,
        //              with an EMPTY change log (no files until `insert`)
        //
        // The new view is a Draft workspace whose edges go to
        // GRAPH (filtered by this view's change set).  The parent link
        // gives the overlay chain read-access to the shared graph for
        // record-time diff computation.
        //
        // When --from is specified, the source's changes are inserted
        // immediately so the new view starts with the source's files.
        // Without --from, the change log starts empty — the user brings
        // in changes explicitly via `insert from-view`.  This is the
        // normal workflow:
        //
        //   atomic view create feature          # empty workspace
        //   atomic insert from-view dev         # inherit dev's files
        //   # ... make changes, record ...
        //   atomic insert from-view feature --to-view dev  # promote
        if let Some(ref source) = self.from {
            // Explicit --from: fork from the specified view.
            if !repo.view_exists(source).map_err(CliError::Repository)? {
                return Err(CliError::ViewNotFound {
                    name: source.to_string(),
                });
            }

            let source_info = repo.get_view_info(source).map_err(CliError::Repository)?;
            let change_count = source_info.change_count;

            // create_stack_from creates a Draft workspace parented on
            // the source, with the source's change log copied over.
            repo.create_view_from(name, source)
                .map_err(CliError::Repository)?;

            if change_count > 0 {
                print_success(&format!(
                    "Created view: {} (forked from {} with {} changes)",
                    style_view(name),
                    style_view(source),
                    change_count,
                ));
            } else {
                print_success(&format!(
                    "Created view: {} (forked from {} - empty)",
                    style_view(name),
                    style_view(source),
                ));
            }
        } else {
            // No --from: create an empty Draft workspace parented on the
            // nearest Shared ancestor.  No changes are inherited — the
            // user inserts them explicitly.
            repo.create_view(name).map_err(CliError::Repository)?;

            print_success(&format!(
                "Created view: {} (forked from {} - empty)",
                style_view(name),
                style_view(
                    &repo
                        .nearest_shared_ancestor(repo.current_view())
                        .unwrap_or_else(|_| repo.current_view().to_string())
                ),
            ));
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
    // View Name Validation Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_valid_view_names() {
        assert!(validate_view_name("main").is_ok());
        assert!(validate_view_name("dev").is_ok());
        assert!(validate_view_name("feature-auth").is_ok());
        assert!(validate_view_name("feature_auth").is_ok());
        assert!(validate_view_name("bugfix-123").is_ok());
        assert!(validate_view_name("release-1.0.0").is_ok());
        assert!(validate_view_name("user@domain").is_ok());
        assert!(validate_view_name("CamelCase").is_ok());
        assert!(validate_view_name("UPPERCASE").is_ok());
        assert!(validate_view_name("a").is_ok());
        assert!(validate_view_name("123").is_ok());
    }

    #[test]
    fn test_empty_name() {
        let result = validate_view_name("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(MAX_VIEW_NAME_LENGTH + 1);
        let result = validate_view_name(&long_name);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("255"));
    }

    #[test]
    fn test_invalid_characters() {
        assert!(validate_view_name("feature/auth").is_err());
        assert!(validate_view_name("feature\\auth").is_err());
        assert!(validate_view_name("feature:auth").is_err());
        assert!(validate_view_name("feature*auth").is_err());
        assert!(validate_view_name("feature?auth").is_err());
        assert!(validate_view_name("feature\"auth").is_err());
        assert!(validate_view_name("feature<auth").is_err());
        assert!(validate_view_name("feature>auth").is_err());
        assert!(validate_view_name("feature|auth").is_err());
        assert!(validate_view_name("feature auth").is_err());
    }

    #[test]
    fn test_reserved_names() {
        assert!(validate_view_name(".").is_err());
        assert!(validate_view_name("..").is_err());
    }

    #[test]
    fn test_dot_restrictions() {
        assert!(validate_view_name(".hidden").is_err());
        assert!(validate_view_name("trailing.").is_err());
        // Dots in the middle are allowed
        assert!(validate_view_name("feature.auth").is_ok());
        assert!(validate_view_name("v1.0.0").is_ok());
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
        let cmd = New::with_name("orphan-view").with_empty(true);
        assert_eq!(cmd.name, Some("orphan-view".to_string()));
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
    fn test_run_creates_view_forked_from_current() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory and create a view
        std::env::set_current_dir(repo_path).unwrap();

        // Create a view without --from (should default to forking from current view)
        let cmd = New::with_name("feature-test");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the view exists and was forked from dev (the default view)
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.view_exists("feature-test").unwrap());

        // Both views should have the same state (since dev is empty, feature-test
        // should also be empty but forked from dev's changelog)
        let dev_info = repo.get_view_info("dev").unwrap();
        let feature_info = repo.get_view_info("feature-test").unwrap();
        assert_eq!(dev_info.change_count, feature_info.change_count);
    }

    #[test]
    #[serial]
    fn test_run_creates_empty_view_with_flag() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create an empty (orphan) view with --empty flag
        let cmd = New::with_name("orphan-view").with_empty(true);
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the view exists
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.view_exists("orphan-view").unwrap());

        // The orphan view should have 0 changes (truly empty, not forked)
        let orphan_info = repo.get_view_info("orphan-view").unwrap();
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

        // Verify we switched to the new view
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "feature-switch");
    }

    #[test]
    #[serial]
    fn test_run_duplicate_view() {
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

        // Create the view first time
        let cmd = New::with_name("duplicate");
        assert!(cmd.run().is_ok());

        // Try to create it again
        let cmd = New::with_name("duplicate");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ViewAlreadyExists { name } => {
                assert_eq!(name, "duplicate");
            }
            other => panic!("Expected ViewAlreadyExists, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_run_with_explicit_from() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a source view
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("source-view").unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Create a view explicitly forked from source-view
        let cmd = New::with_name("forked-view").with_from("source-view");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify the view exists
        let repo = Repository::open(repo_path).unwrap();
        assert!(repo.view_exists("forked-view").unwrap());
    }

    #[test]
    #[serial]
    fn test_run_from_nonexistent_view_fails() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        std::env::set_current_dir(repo_path).unwrap();

        // Try to create a view from a nonexistent source
        let cmd = New::with_name("new-view").with_from("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ViewNotFound { name } => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("Expected ViewNotFound, got: {:?}", other),
        }
    }
}

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
/// A view is a filter over the global graph: every change and node it
/// exposes already lives in the graph. Creation always makes a DRAFT
/// overlay — anchored on a parent — whose change-set membership optionally
/// starts SEEDED from an existing view's membership (`--from`). The new
/// view owns nothing; it selects.
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
    /// Seeds the new view's change-set membership from `<VIEW>`: the view
    /// exposes the source's nodes through its own filter. Nothing is copied
    /// out of the graph — the edges, hunks, and content already live in it —
    /// and the source keeps every one of its nodes. Recording on the new
    /// view afterwards writes draft edges the source cannot see.
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
    /// Draft is the ONLY creation scope — every view is born a draft
    /// (an overlay filter over the graph) and may be promoted to a Shared
    /// root scope with `view promote`. This flag is accepted for clarity
    /// and symmetry.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a draft feature view anchored on dev
    /// atomic view create feature-auth --draft
    ///
    /// # Create a draft workspace with an explicit parent
    /// atomic view create feature-login --draft --parent service-auth
    /// ```
    #[arg(long, short = 'd')]
    pub draft: bool,

    /// Parent view for the new view.
    ///
    /// Anchors the overlay chain: a draft exposes its own change-set plus
    /// its parent's effective set (recursively back to the nearest Shared
    /// view). Every node it exposes already lives in the graph — a child
    /// never copies or owns its parent's nodes.
    ///
    /// Defaults to the nearest Shared ancestor of the current view.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Anchor on a long-lived service view
    /// atomic view create feature-login --parent service-auth
    ///
    /// # Anchor on dev (the default if dev is current)
    /// atomic view create bugfix-123 --parent dev
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

        // ONE creation concept: a Draft overlay whose filter is anchored on a
        // parent and may be SEEDED from a source view's change-set
        // membership.  The graph holds every node a view exposes; --from only
        // selects which EXISTING changes the new view's filter starts with.
        //
        //   --from S              → anchor on S, seed from S
        //   --from S --parent P   → anchor on P, seed from S
        //   --parent P | --draft  → anchor on P, empty membership
        //   (default)             → anchor on nearest Shared, empty membership
        //                           (bring files in with
        //                            `atomic insert from-view dev`)
        if let Some(ref source) = self.from {
            if !repo.view_exists(source).map_err(CliError::Repository)? {
                return Err(CliError::ViewNotFound {
                    name: source.to_string(),
                });
            }
        }

        let (anchor, seed) = resolve_overlay_creation(self.from.as_deref(), self.parent.as_deref());

        let source_info = if let Some(seed_view) = seed.as_deref() {
            Some(
                repo.get_view_info(seed_view)
                    .map_err(CliError::Repository)?,
            )
        } else {
            None
        };

        repo.create_overlay_view(name, anchor.as_deref(), seed.as_deref())
            .map_err(CliError::Repository)?;

        if let Some(info) = source_info {
            if info.change_count > 0 {
                print_success(&format!(
                    "Created view: {} (seeded from {} - {} changes)",
                    style_view(name),
                    style_view(&info.name),
                    info.change_count,
                ));
            } else {
                print_success(&format!(
                    "Created view: {} (seeded from {} - empty)",
                    style_view(name),
                    style_view(&info.name),
                ));
            }
        } else {
            let anchored = if let Some(a) = anchor.as_deref() {
                a.to_string()
            } else {
                match repo.nearest_shared_ancestor(repo.current_view()) {
                    Ok(name) => name,
                    Err(_) => repo.current_view().to_string(),
                }
            };
            print_success(&format!(
                "Created view: {} (empty workspace, anchored on {})",
                style_view(name),
                style_view(&anchored),
            ));
        }

        self.maybe_switch(name, &mut repo)
    }
}

// Tests

/// Resolve how a new view is anchored and seeded.
///
/// Returns `(anchor, seed)`: the overlay-chain anchor view (`None` means the
/// repository default — the nearest Shared ancestor of the current view) and
/// the view whose change-set membership seeds the new view's filter.
///
/// # Model
///
/// A view is a filter over the global graph: every node it exposes already
/// lives in the graph. Creation selects where the overlay chain anchors and,
/// optionally, which EXISTING changes the new view's filter starts with —
/// nothing is copied out of the graph; no node is ever owned by two views.
pub fn resolve_overlay_creation(
    from: Option<&str>,
    parent: Option<&str>,
) -> (Option<String>, Option<String>) {
    // --from anchors on its source unless an explicit --parent overrides;
    // the seed is always the --from source.
    let anchor = parent.or(from).map(|s| s.to_string());
    (anchor, from.map(|s| s.to_string()))
}

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
    // -------------------------------------------------------------------------
    // View kind resolution for the two-tier (--draft/--parent) path
    // -------------------------------------------------------------------------

    #[test]
    fn overlay_creation_anchors_and_seeds() {
        // --from S: anchored on S and seeded from S.
        let (anchor, seed) = resolve_overlay_creation(Some("dev"), None);
        assert_eq!(anchor, Some("dev".into()));
        assert_eq!(seed, Some("dev".into()));

        // --from S --parent P: anchored on P, seeded from S.
        let (anchor, seed) = resolve_overlay_creation(Some("dev"), Some("staging"));
        assert_eq!(anchor, Some("staging".into()));
        assert_eq!(seed, Some("dev".into()));

        // --parent P only: anchored on P, empty membership.
        let (anchor, seed) = resolve_overlay_creation(None, Some("staging"));
        assert_eq!(anchor, Some("staging".into()));
        assert_eq!(seed, None);

        // default: repository chooses the nearest Shared ancestor; no seed.
        let (anchor, seed) = resolve_overlay_creation(None, None);
        assert_eq!(anchor, None);
        assert_eq!(seed, None);
    }
}

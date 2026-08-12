//! The `view switch` command for switching between views.
//!
//! This module implements the `atomic view switch` command, which changes
//! the current view to a different one and updates the working copy to
//! match the new view's state.
//!
//! # Working Copy Update
//!
//! Like Pijul's channel switching, switching views in Atomic updates the
//! working copy to reflect the new view's state. Files are created, updated,
//! or removed to match what's recorded in the target view.
//!
//! # Usage
//!
//! ```text
//! atomic view switch <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the view to switch to
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Switch to a different view:
//! ```text
//! $ atomic view switch feature-auth
//! Switched to view: feature-auth
//! ```
//!
//! Switch back to dev:
//! ```text
//! $ atomic view switch dev
//! Switched to view: dev
//! ```

use clap::Parser;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::complete::complete_view_names;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, view as style_view};

#[cfg(test)]
use std::path::PathBuf;

// Switch Command

/// Switch to a different view.
///
/// Changes the current view to the specified one and updates the working
/// copy to match the new view's state. This behavior matches Pijul's
/// channel switching.
///
/// **Note**: Switching views WILL update your working copy files to match
/// the target view's state. Unrecorded changes may be overwritten.
#[derive(Parser, Debug, Default)]
#[command(name = "switch")]
pub struct Switch {
    /// Name of the view to switch to.
    ///
    /// The view must already exist. Use `atomic view list` to see
    /// available views, or `atomic view create` to create a new one.
    #[arg(value_name = "NAME", add = ArgValueCompleter::new(complete_view_names))]
    pub name: Option<String>,

    /// Force switch even with unrecorded changes.
    ///
    /// By default, switching views is blocked when the working copy has
    /// uncommitted changes (modified, added, or deleted files). This
    /// prevents accidental loss of work during materialization.
    ///
    /// Use `--force` to override this check. Unrecorded changes will
    /// be overwritten by the target view's content.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Stash unrecorded changes before switching.
    ///
    /// Automatically runs `atomic stash push` to save your uncommitted
    /// changes, then switches views. Use `atomic stash pop` after
    /// switching back to restore your changes.
    #[arg(long, short = 's')]
    pub stash: bool,
}

impl Switch {
    /// Create a new Switch command targeting the given view.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            force: false,
            stash: false,
        }
    }
}

impl Command for Switch {
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

        // Check if we're already on this view
        if repo.current_view() == name {
            print_success(&format!("Already on view: {}", style_view(name)));
            return Ok(());
        }

        // Block switch if working copy has unrecorded changes
        if !self.force {
            let status = repo
                .status(atomic_repository::StatusOptions::default())
                .map_err(CliError::Repository)?;

            if !status.is_clean() {
                if self.stash {
                    // Auto-stash: save changes before switching
                    let stash_cmd = crate::commands::stash::Stash::new()
                        .with_message(format!("Auto-stash before switching to {}", name));
                    stash_cmd.run_push_on(&mut repo, None, false, false)?;
                } else {
                    let dirty: Vec<String> = status
                        .entries()
                        .iter()
                        .filter(|e| e.status().is_dirty())
                        .map(|e| format!("  {} {}", e.status().short_code(), e.path().display()))
                        .collect();

                    crate::output::print_error(&format!(
                        "Cannot switch views with unrecorded changes ({} file{}):",
                        dirty.len(),
                        if dirty.len() == 1 { "" } else { "s" },
                    ));
                    for line in &dirty {
                        eprintln!("{}", line);
                    }
                    eprintln!();
                    eprintln!("Use 'atomic record' to save changes, 'atomic view switch --stash' to stash them, or '--force' to discard them.");
                    return Err(CliError::InvalidArgument {
                        message: "Working copy has unrecorded changes".to_string(),
                    });
                }
            }
        }

        // Switch to the view and update working copy
        let spinner = crate::output::create_spinner("Materializing files for view...");

        let result = repo.switch_view(name).map_err(|e| match e {
            atomic_repository::RepositoryError::ViewNotFound { name } => {
                CliError::ViewNotFound { name }
            }
            other => CliError::Repository(other),
        })?;

        crate::output::finish_success(
            &spinner,
            &format!(
                "Switched to view: {} ({} files updated, {} directories)",
                style_view(name),
                result.files_written,
                result.directories_created,
            ),
        );

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
    fn test_switch_to_existing_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature-test").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the new view
        let cmd = Switch::with_name("feature-test");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we switched
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "feature-test");
    }

    #[test]
    #[serial]
    fn test_switch_to_nonexistent_view() {
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

        // Try to switch to a non-existent view
        let cmd = Switch::with_name("nonexistent");
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
    fn test_switch_to_current_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository (default view is "dev") and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the current view (should succeed with a message)
        let cmd = Switch::with_name("dev");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on dev
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "dev");
    }
}

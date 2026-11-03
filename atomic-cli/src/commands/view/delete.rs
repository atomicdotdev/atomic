//! The `view delete` command for deleting a view.
//!
//! This module implements the `atomic view delete` command, which removes
//! a view from the repository. The view's metadata is deleted, but the
//! changes themselves remain in the graph (they may be referenced by other
//! views).
//! If deletion would orphan changes, their hashes are shown and confirmation
//! is required. `--force` explicitly skips that confirmation.
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
use clap_complete::engine::ArgValueCompleter;

use crate::commands::complete::complete_view_names;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{
    hash as style_hash, print_hint, print_success, print_warning, view as style_view,
};
use atomic_core::types::Base32;

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
    #[arg(value_name = "NAME", add = ArgValueCompleter::new(complete_view_names))]
    pub name: Option<String>,

    /// Force deletion without confirmation.
    ///
    /// By default, confirmation is required if deletion would orphan changes.
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

fn confirm_in_terminal(prompt: &str) -> CliResult<bool> {
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()
        .map_err(|_| CliError::InvalidArgument {
            message: "Deleting this view would orphan changes and requires confirmation. Re-run with --force to acknowledge the risks and proceed non-interactively.".to_string(),
        })
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

        // The service executes a dispatched delete directly, so orphan
        // warnings and the acknowledgement are surfaced client-side BEFORE
        // any dispatch. The analysis is best-effort: a refusal to proceed
        // (cancel / no terminal) is final, but any other analysis failure
        // is deferred so the dispatch or local execution below surfaces
        // the canonical error.
        let pre_confirmed = !self.force;
        if pre_confirmed {
            let gate = match find_repository_root() {
                Ok(repo_root) => match Repository::open(&repo_root) {
                    Ok(repo) => self.validate_and_warn(&repo, name, true, confirm_in_terminal),
                    Err(_) => Ok(()),
                },
                Err(e) => Err(e),
            };
            if let Err(e @ (CliError::Cancelled | CliError::InvalidArgument { .. })) = gate {
                return Err(e);
            }
        }

        // Route through the daemon when reachable.
        if crate::commands::rpc::view_delete(self)? {
            return Ok(());
        }

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        self.execute_with_repo_gated(&mut repo, name, pre_confirmed, confirm_in_terminal)
    }
}

impl Delete {
    fn execute_with_repo<F>(&self, repo: &mut Repository, name: &str, confirm: F) -> CliResult<()>
    where
        F: FnOnce(&str) -> CliResult<bool>,
    {
        self.execute_with_repo_gated(repo, name, false, confirm)
    }

    /// Validations that must hold on every path, plus (optionally) the
    /// orphan warning and acknowledgement prompt.
    fn validate_and_warn<F>(
        &self,
        repo: &Repository,
        name: &str,
        warn_orphans: bool,
        confirm: F,
    ) -> CliResult<()>
    where
        F: FnOnce(&str) -> CliResult<bool>,
    {
        // Check if trying to delete the working copy's desired view
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let desired_view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;
        if desired_view == name {
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

        if !warn_orphans {
            return Ok(());
        }

        let orphans = repo
            .view_deletion_orphans(name)
            .map_err(CliError::Repository)?;
        if !orphans.is_empty() {
            print_warning(&format!(
                "Deleting '{}' would orphan {} change(s): no remaining view references them.",
                name,
                orphans.len(),
            ));
            for hash in &orphans {
                println!("  {}", style_hash(hash.to_base32()));
            }
            print_warning(
                "Their data is not erased by view deletion, but they will disappear from all views and will not return to the source automatically.",
            );
            print_hint("Insert these changes into another view before deleting this one to keep them referenced.");
            if !self.force
                && !confirm("I understand these changes will be orphaned; delete the view")?
            {
                return Err(CliError::Cancelled);
            }
        }

        Ok(())
    }

    fn execute_with_repo_gated<F>(
        &self,
        repo: &mut Repository,
        name: &str,
        pre_confirmed: bool,
        confirm: F,
    ) -> CliResult<()>
    where
        F: FnOnce(&str) -> CliResult<bool>,
    {
        self.validate_and_warn(repo, name, !pre_confirmed, confirm)?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;

        // Capture scope before deletion: the mapping reconciliation depends
        // on the view's scope (CB-10A).
        let scope = repo.get_view_info(name).ok().map(|info| info.scope);

        // CB-10A review R5: deletion eligibility is decided FIRST and only a
        // successful deletion may touch mappings. `delete_view` refuses
        // Shared views (permanent) and views with children; calling it before
        // any mapping effect means a refused delete leaves the mapping bytes,
        // Git refs, view rows and operation state untouched.
        repo.delete_view(name).map_err(|e| match e {
            atomic_repository::RepositoryError::ViewNotFound { name } => {
                CliError::ViewNotFound { name }
            }
            atomic_repository::RepositoryError::CannotDeleteCurrentView { name } => {
                CliError::CannotDeleteCurrentView { name }
            }
            other => CliError::Repository(other),
        })?;

        // CB-10A: a Draft's mapping row is removed together with the (now
        // deleted) view. A Shared view can never reach this point: deletion
        // is refused above and its mapping stays intact.
        if let Some(atomic_core::pristine::ViewScope::Draft) = scope {
            repo.reconcile_mapping_after_view_delete(
                working_copy,
                name,
                atomic_core::pristine::ViewScope::Draft,
            )
            .map_err(CliError::Repository)?;
        }

        print_success(&format!("Deleted view: {}", style_view(name)));

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::file_serial;

    fn orphan_repo() -> (tempfile::TempDir, Repository, atomic_core::types::Hash) {
        use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let hash = atomic_core::types::Hash::of(b"unique change");
        let mut txn = repo.pristine().write_txn().unwrap();
        let parent = txn.get_view(repo.current_view()).unwrap().unwrap();
        let mut draft = txn
            .create_view("wip", ViewScope::Draft, Some(parent.id))
            .unwrap();
        let id = txn.register_change(&hash).unwrap();
        txn.put_change(&mut draft, id, &hash).unwrap();
        txn.put_change_deps(id, &[]).unwrap();
        txn.update_view(&draft).unwrap();
        txn.commit().unwrap();
        (temp, repo, hash)
    }

    #[test]
    fn declining_or_failed_confirmation_keeps_orphan_candidate_referenced() {
        let (_temp, mut repo, hash) = orphan_repo();
        let cmd = Delete::with_name("wip");
        let result = cmd.execute_with_repo(&mut repo, "wip", |prompt| {
            assert!(prompt.contains("orphaned"));
            Ok(false)
        });
        assert!(matches!(result, Err(CliError::Cancelled)));
        let result = cmd.execute_with_repo(&mut repo, "wip", |_| {
            Err(CliError::InvalidArgument {
                message: "no terminal".to_string(),
            })
        });
        assert!(result.is_err());
        assert!(repo.view_exists("wip").unwrap());
        assert_eq!(repo.views_containing_change(&hash).unwrap(), vec!["wip"]);
    }

    #[test]
    fn confirming_or_forcing_allows_orphaning() {
        for force in [false, true] {
            let (_temp, mut repo, hash) = orphan_repo();
            Delete::with_name("wip")
                .with_force(force)
                .execute_with_repo(&mut repo, "wip", |_| {
                    assert!(!force, "--force must skip the prompt");
                    Ok(true)
                })
                .unwrap();
            assert!(!repo.view_exists("wip").unwrap());
            assert!(repo.views_containing_change(&hash).unwrap().is_empty());
        }
    }

    #[test]
    fn changes_retained_elsewhere_allow_deletion_without_prompt() {
        use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
        let (_temp, mut repo, hash) = orphan_repo();
        let mut txn = repo.pristine().write_txn().unwrap();
        let mut parent = txn.get_view(repo.current_view()).unwrap().unwrap();
        let id = txn.get_internal(&hash).unwrap().unwrap();
        txn.put_change(&mut parent, id, &hash).unwrap();
        txn.update_view(&parent).unwrap();
        txn.commit().unwrap();
        Delete::with_name("wip")
            .execute_with_repo(&mut repo, "wip", |_| panic!("no changes would be orphaned"))
            .unwrap();
        assert_eq!(
            repo.views_containing_change(&hash).unwrap(),
            vec![repo.current_view()]
        );
    }

    #[test]
    fn parent_view_deletion_is_rejected_before_prompting() {
        let (_temp, mut repo, _hash) = orphan_repo();
        repo.create_overlay_view("child", Some("wip"), None)
            .unwrap();
        let result = Delete::with_name("wip")
            .execute_with_repo(&mut repo, "wip", |_| panic!("cannot delete a parent view"));
        assert!(matches!(
            result,
            Err(CliError::Repository(
                atomic_repository::RepositoryError::InvalidOperation { .. }
            ))
        ));
        assert!(repo.view_exists("wip").unwrap());
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
    #[file_serial]
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
    #[file_serial]
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

        // Try to delete a non-existent view. The plain form routes
        // through the service area; the refusal is the domain's.
        let cmd = Delete::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ServiceRefusal { message } => {
                assert!(
                    message.contains("view 'nonexistent' not found"),
                    "unexpected refusal: {message}"
                );
            }
            other => panic!("Expected the view-not-found refusal, got: {:?}", other),
        }
    }

    #[test]
    #[file_serial]
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

        // Try to delete the current view. The plain form routes through
        // the service area (local backend by default): the domain's
        // refusal rides the contract's error vocabulary.
        let cmd = Delete::with_name("dev");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ServiceRefusal { message } => {
                assert!(
                    message.contains("cannot delete the current view 'dev'"),
                    "unexpected refusal: {message}"
                );
            }
            other => panic!("Expected the current-view refusal, got: {:?}", other),
        }
    }

    #[test]
    #[file_serial]
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

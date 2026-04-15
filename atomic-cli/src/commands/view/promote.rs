//! The `view promote` command for changing a view to Shared scope.
//!
//! This is useful after `git import` which may create Draft views,
//! or when a Draft view should become a permanent Shared view.
//!
//! Promoting a view to Shared with no parent enables the fast
//! `filter_is_universal` path in `status`, avoiding the O(N)
//! change-loading slow path.

use clap::Parser;

use atomic_core::pristine::ViewScope;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success};

/// Promote a view to Shared scope with no parent.
///
/// This changes the view's scope to Shared and clears its parent,
/// making it a root view. This enables fast status operations by
/// skipping the per-change view filter.
///
/// # Examples
///
/// ```text
/// # Promote the current view
/// atomic view promote
///
/// # Promote a specific view
/// atomic view promote master
/// ```
#[derive(Parser, Debug)]
pub struct Promote {
    /// Name of the view to promote. Defaults to the current view.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

impl Command for Promote {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;

        let view_name = match &self.name {
            Some(name) => name.clone(),
            None => repo.current_view().to_string(),
        };

        let info = repo
            .get_view_info(&view_name)
            .map_err(|e| CliError::Internal(e.into()))?;

        if info.scope.is_shared() && info.parent_name.is_none() {
            print_info(&format!(
                "View '{}' is already a shared root view",
                view_name
            ));
            return Ok(());
        }

        repo.set_view_scope(&view_name, ViewScope::Shared)
            .map_err(|e| CliError::Internal(e.into()))?;

        print_success(&format!(
            "Promoted '{}' to shared root view (was {:?}, parent: {:?})",
            view_name,
            info.scope,
            info.parent_name.as_deref().unwrap_or("none"),
        ));

        Ok(())
    }
}

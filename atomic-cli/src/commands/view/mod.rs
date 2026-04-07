//! View management commands for Atomic VCS.
//!
//! Views are one of Atomic's most powerful features, enabling parallel development
//! lines that can be managed independently. Unlike Git branches, views are
//! **first-class citizens** in Atomic's data model - they represent perspectives on the
//! same underlying graph, not divergent histories.
//!
//! # Key Characteristics
//!
//! - **Independent Evolution**: Each view maintains its own sequence of changes
//! - **Selective Merging**: Pull specific changes between views without complex rebasing
//! - **Conflict-Free by Design**: Atomic's patch theory handles most conflicts automatically
//! - **Named and Discoverable**: Views have human-readable names and can be listed/queried
//!
//! # Critical Difference: Views are NOT Git Branches
//!
//! **Views share the same working copy** - they are NOT isolated filesystems like Git branches.
//!
//! When you switch between views:
//! - ✅ **Recorded changes** are applied/unapplied (patch history is isolated)
//! - ✅ **Unrecorded changes** remain in your working copy (files persist)
//! - ❌ **Untracked files** do NOT disappear (unlike `git checkout`)
//!
//! Think of views as **different perspectives on patch history** operating on the **same workspace**.
//!
//! # Usage
//!
//! ```text
//! atomic view <COMMAND>
//!
//! Commands:
//!   create  Create a new view
//!   switch  Switch to a different view
//!   delete  Delete a view
//!   list    List all views
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ## Creating and Switching Views
//!
//! ```text
//! # Create a new feature view
//! $ atomic view create feature-auth
//! Created view: feature-auth
//!
//! # Switch to the feature view
//! $ atomic view switch feature-auth
//! Switched to view: feature-auth
//!
//! # List all views
//! $ atomic view list
//!   dev
//! * feature-auth
//! ```
//!
//! ## Feature Development Workflow
//!
//! ```text
//! # Create and switch to a feature view
//! $ atomic view create feature-auth --switch
//! Created view: feature-auth
//! Switched to view: feature-auth
//!
//! # Work on your feature
//! $ atomic record -m "Add authentication module"
//! $ atomic record -m "Add login endpoint"
//!
//! # Switch back to main development
//! $ atomic view switch dev
//! Switched to view: dev
//! ```
//!
//! ## Cleanup Old Views
//!
//! ```text
//! # Delete a merged feature view
//! $ atomic view delete feature-old
//! Deleted view: feature-old
//!
//! # Force delete if needed
//! $ atomic view delete experiment --force
//! Deleted view: experiment
//! ```

pub mod delete;
pub mod list;
pub mod new;
pub mod switch;

use clap::Subcommand;

pub use delete::Delete;
pub use list::List;
pub use new::New;
pub use switch::Switch;

use crate::commands::Command;
use crate::error::CliResult;

// View Subcommands

/// Subcommands for view management.
///
/// Views in Atomic are similar to branches in Git, but they represent
/// perspectives on the same underlying graph rather than divergent histories.
#[derive(Subcommand, Debug)]
pub enum ViewCommands {
    /// Create a new view.
    ///
    /// Creates a new view, optionally based on the current view's state.
    /// By default, the new view starts with the same changes as the current view.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a new view
    /// atomic view create feature-auth
    ///
    /// # Create and switch to the new view
    /// atomic view create feature-auth --switch
    /// ```
    #[command(name = "create")]
    Create(New),

    /// Switch to a different view.
    ///
    /// Changes the current view to the specified one. This updates which
    /// perspective on the graph you're working with, but doesn't change the
    /// files in your working copy.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic view switch dev
    /// atomic view switch feature-auth
    /// ```
    Switch(Switch),

    /// Delete a view.
    ///
    /// Removes the specified view. You cannot delete the current view -
    /// switch to a different view first.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic view delete old-feature
    /// atomic view delete experiment --force
    /// ```
    Delete(Delete),

    /// List all views.
    ///
    /// Shows all views in the repository. The current view is marked
    /// with an asterisk (*).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic view list
    /// atomic view list --verbose
    /// ```
    List(List),
}

// View Command

/// View management command.
///
/// This is the top-level command that dispatches to subcommands.
#[derive(Debug, clap::Args)]
#[command(name = "view")]
pub struct View {
    #[command(subcommand)]
    pub command: ViewCommands,
}

impl Command for View {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            ViewCommands::Create(cmd) => cmd.run(),
            ViewCommands::Switch(cmd) => cmd.run(),
            ViewCommands::Delete(cmd) => cmd.run(),
            ViewCommands::List(cmd) => cmd.run(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_commands_variants() {
        // Ensure all variants exist and can be matched
        fn check_variant(cmd: &ViewCommands) -> &'static str {
            match cmd {
                ViewCommands::Create(_) => "create",
                ViewCommands::Switch(_) => "switch",
                ViewCommands::Delete(_) => "delete",
                ViewCommands::List(_) => "list",
            }
        }

        // Create instances of each variant
        let new = New::default();
        let switch = Switch::default();
        let delete = Delete::default();
        let list = List::default();

        assert_eq!(check_variant(&ViewCommands::Create(new)), "create");
        assert_eq!(check_variant(&ViewCommands::Switch(switch)), "switch");
        assert_eq!(check_variant(&ViewCommands::Delete(delete)), "delete");
        assert_eq!(check_variant(&ViewCommands::List(list)), "list");
    }
}

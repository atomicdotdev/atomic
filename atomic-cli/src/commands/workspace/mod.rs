//! Workspace management commands for remote atomic-storage servers.
//!
//! Workspaces are organisational containers for projects on a remote
//! atomic-storage server. They provide isolation, access control, and
//! a human-readable URL namespace.
//!
//! # Usage
//!
//! ```text
//! atomic workspace <COMMAND>
//!
//! Commands:
//!   list    List remote workspaces
//!   create  Create a new remote workspace
//!   show    Show workspace details
//!   update  Update a workspace
//!   delete  Delete a workspace
//!   set     Set the default workspace for an org
//!   grant   Manage workspace permission grants
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all workspaces
//! $ atomic workspace list
//! NAME          SLUG          VISIBILITY  CREATED
//! ─────────────────────────────────────────────────
//! My Team       my-team       private     2025-06-01 10:00:00
//! Open Source    open-source   public      2025-05-15 09:30:00
//!
//! # Create a workspace
//! $ atomic workspace create "My Team" --visibility private
//! ✓ Created workspace: my-team
//!
//! # Show workspace details
//! $ atomic workspace show my-team
//! Name:        My Team
//! Slug:        my-team
//! Visibility:  private
//! Created:     2025-06-01 10:00:00
//!
//! # Delete a workspace
//! $ atomic workspace delete my-team --force
//! ✓ Deleted workspace: my-team
//!
//! # Set the default workspace for the current org
//! $ atomic workspace set my-team
//! ✓ Default workspace for 'acme' set to: my-team
//! ```

pub mod create;
pub mod delete;
pub mod grant;
pub mod list;
pub mod set;
pub mod show;
pub mod update;

use clap::Subcommand;

pub use create::WorkspaceCreate;
pub use delete::WorkspaceDelete;
pub use grant::GrantCmd;
pub use list::WorkspaceList;
pub use set::WorkspaceSet;
pub use show::WorkspaceShow;
pub use update::WorkspaceUpdate;

use crate::commands::Command;
use crate::error::CliResult;

/// Available workspace subcommands.
#[derive(Subcommand, Debug)]
pub enum WorkspaceCommands {
    /// List remote workspaces.
    ///
    /// Shows all workspaces visible to the authenticated identity on the
    /// configured atomic-storage server.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace list
    /// atomic workspace list --format json
    /// atomic workspace list --org acme
    /// ```
    List(WorkspaceList),

    /// Create a new remote workspace.
    ///
    /// Creates a workspace on the configured atomic-storage server. The
    /// workspace slug is derived from the name automatically.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace create "My Team"
    /// atomic workspace create "My Team" --visibility public
    /// atomic workspace create "My Team" --description "Team workspace"
    /// ```
    Create(WorkspaceCreate),

    /// Show workspace details.
    ///
    /// Displays detailed information about a specific workspace identified
    /// by its slug.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace show my-team
    /// atomic workspace show my-team --format json
    /// ```
    Show(WorkspaceShow),

    /// Update a workspace.
    ///
    /// Modifies one or more properties of an existing workspace.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace update my-team --name "New Name"
    /// atomic workspace update my-team --visibility public
    /// ```
    Update(WorkspaceUpdate),

    /// Delete a workspace.
    ///
    /// Removes a workspace from the remote server. This is destructive —
    /// all projects within the workspace will also be deleted.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace delete my-team
    /// atomic workspace delete my-team --force
    /// ```
    Delete(WorkspaceDelete),

    /// Set the default workspace for an org.
    ///
    /// Stores the chosen workspace slug in `[server.default_workspaces]`
    /// keyed by the target org. Subsequent commands that take an optional
    /// `--workspace` parameter fall back to this default.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace set my-team
    /// atomic workspace set personal --org alice
    /// ```
    Set(WorkspaceSet),

    /// Manage workspace permission grants.
    ///
    /// Grant or revoke access to a workspace for a team or user.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace grant list my-team
    /// atomic workspace grant add my-team --team engineering --permission write
    /// atomic workspace grant remove my-team --team engineering
    /// ```
    Grant(GrantCmd),
}

/// Workspace management command.
///
/// Top-level dispatcher that routes to workspace subcommands.
#[derive(Debug, clap::Args)]
#[command(name = "workspace", visible_alias = "ws")]
pub struct WorkspaceCmd {
    /// The workspace subcommand to run.
    #[command(subcommand)]
    pub command: WorkspaceCommands,
}

impl Command for WorkspaceCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            WorkspaceCommands::List(cmd) => cmd.run(),
            WorkspaceCommands::Create(cmd) => cmd.run(),
            WorkspaceCommands::Show(cmd) => cmd.run(),
            WorkspaceCommands::Update(cmd) => cmd.run(),
            WorkspaceCommands::Delete(cmd) => cmd.run(),
            WorkspaceCommands::Set(cmd) => cmd.run(),
            WorkspaceCommands::Grant(cmd) => cmd.run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_commands_variants() {
        fn check_variant(cmd: &WorkspaceCommands) -> &'static str {
            match cmd {
                WorkspaceCommands::List(_) => "list",
                WorkspaceCommands::Create(_) => "create",
                WorkspaceCommands::Show(_) => "show",
                WorkspaceCommands::Update(_) => "update",
                WorkspaceCommands::Delete(_) => "delete",
                WorkspaceCommands::Set(_) => "set",
                WorkspaceCommands::Grant(_) => "grant",
            }
        }

        let list = WorkspaceList::default();
        let create = WorkspaceCreate::default();
        let show = WorkspaceShow::default();
        let update = WorkspaceUpdate::default();
        let delete = WorkspaceDelete::default();
        let set = WorkspaceSet::default();

        assert_eq!(check_variant(&WorkspaceCommands::List(list)), "list");
        assert_eq!(check_variant(&WorkspaceCommands::Create(create)), "create");
        assert_eq!(check_variant(&WorkspaceCommands::Show(show)), "show");
        assert_eq!(check_variant(&WorkspaceCommands::Update(update)), "update");
        assert_eq!(check_variant(&WorkspaceCommands::Delete(delete)), "delete");
        assert_eq!(check_variant(&WorkspaceCommands::Set(set)), "set");
    }
}

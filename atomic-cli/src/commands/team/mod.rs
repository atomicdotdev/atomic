//! Team management commands for the Atomic CLI.
//!
//! This module provides commands for managing teams within an organization
//! on a remote atomic-storage server, including:
//!
//! - Listing teams in an organization
//! - Creating new teams
//! - Showing team details
//! - Updating team settings
//! - Deleting teams
//! - Managing team members (nested subcommands)
//!
//! # Usage
//!
//! ```text
//! atomic team <COMMAND>
//!
//! Commands:
//!   list    List teams in the organization
//!   create  Create a new team
//!   show    Show team details
//!   update  Update team settings
//!   delete  Delete a team
//!   member  Manage team members
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all teams
//! $ atomic team list
//!
//! # Create a new team
//! $ atomic team create "Backend Engineering" --visibility visible
//!
//! # Show team details
//! $ atomic team show backend-engineering
//!
//! # Add a member to a team
//! $ atomic team member add backend-eng 550e8400-e29b-41d4-a716-446655440000
//! ```

#[cfg(feature = "teams")]
pub mod create;
#[cfg(feature = "teams")]
pub mod delete;
#[cfg(feature = "teams")]
pub mod list;
#[cfg(feature = "teams")]
pub mod member;
#[cfg(feature = "teams")]
pub mod show;
#[cfg(feature = "teams")]
pub mod update;

#[cfg(feature = "teams")]
use clap::Subcommand;

#[cfg(feature = "teams")]
use crate::commands::Command;
#[cfg(feature = "teams")]
use crate::error::CliResult;

/// Available team subcommands.
#[cfg(feature = "teams")]
#[derive(Subcommand, Debug)]
pub enum TeamCommands {
    /// List teams in the organization.
    ///
    /// Shows all teams visible to the caller within the current
    /// organization. Secret teams are only shown to members and
    /// org admins.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team list
    /// atomic team list --org acme-corp
    /// atomic team list --format json
    /// ```
    List(list::TeamList),

    /// Create a new team.
    ///
    /// Creates a new team within the current organization. The slug
    /// is derived server-side from the display name.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team create "Backend Engineering"
    /// atomic team create "Secret Ops" --visibility secret
    /// atomic team create "Frontend" --description "Frontend engineers"
    /// ```
    Create(create::TeamCreate),

    /// Show team details.
    ///
    /// Displays metadata for a specific team including name, slug,
    /// visibility, and description.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team show backend-engineering
    /// atomic team show backend-engineering --format json
    /// ```
    Show(show::TeamShow),

    /// Update team settings.
    ///
    /// Modify the display name, description, or visibility of a team.
    /// Only fields that are provided will be updated.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team update backend-eng --name "Backend Eng"
    /// atomic team update backend-eng --visibility secret
    /// ```
    Update(update::TeamUpdate),

    /// Delete a team.
    ///
    /// Permanently removes a team and all associated memberships and
    /// team-scoped grants. Use --force to skip confirmation.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team delete old-team
    /// atomic team delete old-team --force
    /// ```
    Delete(delete::TeamDelete),

    /// Manage team members.
    ///
    /// Add, remove, and update member roles within a team.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team member list backend-eng
    /// atomic team member add backend-eng <identity-id>
    /// atomic team member remove backend-eng <identity-id>
    /// ```
    Member(member::TeamMemberCmd),
}

/// Manage teams within an organization.
///
/// Teams are groups of identities within an organization. They can be
/// used to manage permissions via grants and to organize members into
/// functional groups (e.g. "backend", "frontend", "devops").
///
/// Teams can be `visible` (discoverable by all org members) or `secret`
/// (only visible to team members and org admins).
#[cfg(feature = "teams")]
#[derive(Debug, clap::Args)]
#[command(name = "team")]
pub struct TeamCmd {
    /// The team subcommand to run.
    #[command(subcommand)]
    pub command: TeamCommands,
}

#[cfg(feature = "teams")]
impl Command for TeamCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            TeamCommands::List(cmd) => cmd.run(),
            TeamCommands::Create(cmd) => cmd.run(),
            TeamCommands::Show(cmd) => cmd.run(),
            TeamCommands::Update(cmd) => cmd.run(),
            TeamCommands::Delete(cmd) => cmd.run(),
            TeamCommands::Member(cmd) => cmd.run(),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "teams")]
mod tests {
    use super::*;

    #[test]
    fn test_team_commands_variants() {
        fn check_variant(cmd: &TeamCommands) -> &'static str {
            match cmd {
                TeamCommands::List(_) => "list",
                TeamCommands::Create(_) => "create",
                TeamCommands::Show(_) => "show",
                TeamCommands::Update(_) => "update",
                TeamCommands::Delete(_) => "delete",
                TeamCommands::Member(_) => "member",
            }
        }

        let list = list::TeamList {
            org: None,
            format: None,
        };
        assert_eq!(check_variant(&TeamCommands::List(list)), "list");

        let create = create::TeamCreate {
            name: "Backend".to_string(),
            description: None,
            visibility: None,
            org: None,
        };
        assert_eq!(check_variant(&TeamCommands::Create(create)), "create");

        let show = show::TeamShow {
            slug: "backend".to_string(),
            org: None,
            format: None,
        };
        assert_eq!(check_variant(&TeamCommands::Show(show)), "show");
    }
}

//! Organization management commands for the Atomic CLI.
//!
//! This module provides commands for managing organizations on a remote
//! atomic-storage server, including:
//!
//! - Showing organization details
//! - Creating new team organizations
//! - Listing organizations you belong to
//! - Updating organization settings
//! - Deleting organizations
//! - Upgrading personal orgs to team orgs
//! - Setting the default organization
//! - Managing organization members (nested subcommands)
//!
//! # Usage
//!
//! ```text
//! atomic org <COMMAND>
//!
//! Commands:
//!   show     Show organization details
//!   create   Create a new team organization
//!   list     List organizations you belong to
//!   update   Update organization settings
//!   delete   Delete an organization
//!   upgrade  Upgrade a personal org to a team org
//!   set      Set the default organization
//!   member   Manage organization members
//! ```
//!
//! # Examples
//!
//! ```text
//! # Show current org details
//! $ atomic org show
//!
//! # Create a new team org
//! $ atomic org create "Acme Corp" --email admin@acme.com
//!
//! # Set default org (accepts `switch` as a hidden alias for back-compat)
//! $ atomic org set acme-corp
//!
//! # Add a member
//! $ atomic org member add 550e8400-e29b-41d4-a716-446655440000 --role admin
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
pub mod set;
#[cfg(feature = "teams")]
pub mod show;
#[cfg(feature = "teams")]
pub mod update;
#[cfg(feature = "teams")]
pub mod upgrade;

#[cfg(feature = "teams")]
use clap::Subcommand;

#[cfg(feature = "teams")]
use crate::commands::Command;
#[cfg(feature = "teams")]
use crate::error::CliResult;

/// Available organization subcommands.
#[cfg(feature = "teams")]
#[derive(Subcommand, Debug)]
pub enum OrgCommands {
    /// Show organization details.
    ///
    /// Displays metadata for an organization including name, slug, plan,
    /// and contact email. Defaults to the current default organization.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show current org
    /// atomic org show
    ///
    /// # Show a specific org
    /// atomic org show acme-corp
    ///
    /// # JSON output
    /// atomic org show --format json
    /// ```
    Show(show::OrgShow),

    /// Create a new team organization.
    ///
    /// Creates a new organization on the remote server. The slug is derived
    /// server-side from the display name.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org create "Acme Corp"
    /// atomic org create "Acme Corp" --email admin@acme.com
    /// ```
    Create(create::OrgCreate),

    /// List organizations you belong to.
    ///
    /// Shows the current default organization and hints on how to switch.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org list
    /// ```
    List(list::OrgList),

    /// Update organization settings.
    ///
    /// Modify the display name or contact email for an organization.
    /// Only fields that are provided will be updated.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org update --name "New Name"
    /// atomic org update acme --email new@acme.com
    /// ```
    Update(update::OrgUpdate),

    /// Delete an organization.
    ///
    /// Permanently removes an organization and all associated data.
    /// Requires owner permissions. Use --force to skip confirmation.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org delete acme-corp
    /// atomic org delete acme-corp --force
    /// ```
    Delete(delete::OrgDelete),

    /// Upgrade a personal org to a team org.
    ///
    /// Converts a personal organization into a team organization,
    /// enabling multi-member collaboration features.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org upgrade
    /// atomic org upgrade my-personal-org
    /// ```
    Upgrade(upgrade::OrgUpgrade),

    /// Set the default organization.
    ///
    /// Changes which organization is used by default for all commands
    /// that interact with the remote server. Accepts `switch` as a
    /// hidden alias for backward compatibility.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org set acme-corp
    /// ```
    Set(set::OrgSet),

    /// Manage organization members.
    ///
    /// Add, remove, and update member roles within an organization.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org member list
    /// atomic org member add <identity-id> --role admin
    /// atomic org member remove <identity-id>
    /// ```
    Member(member::MemberCmd),
}

/// Manage organizations and members.
///
/// Organizations are the top-level grouping for teams, projects, and
/// workspaces on an atomic-storage server. Each user has a personal
/// organization created during registration, and can create or join
/// team organizations for collaboration.
#[cfg(feature = "teams")]
#[derive(Debug, clap::Args)]
#[command(name = "org")]
pub struct OrgCmd {
    /// The organization subcommand to run.
    #[command(subcommand)]
    pub command: OrgCommands,
}

#[cfg(feature = "teams")]
impl Command for OrgCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            OrgCommands::Show(cmd) => cmd.run(),
            OrgCommands::Create(cmd) => cmd.run(),
            OrgCommands::List(cmd) => cmd.run(),
            OrgCommands::Update(cmd) => cmd.run(),
            OrgCommands::Delete(cmd) => cmd.run(),
            OrgCommands::Upgrade(cmd) => cmd.run(),
            OrgCommands::Set(cmd) => cmd.run(),
            OrgCommands::Member(cmd) => cmd.run(),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "teams")]
mod tests {
    use super::*;

    #[test]
    fn test_org_commands_variants() {
        fn check_variant(cmd: &OrgCommands) -> &'static str {
            match cmd {
                OrgCommands::Show(_) => "show",
                OrgCommands::Create(_) => "create",
                OrgCommands::List(_) => "list",
                OrgCommands::Update(_) => "update",
                OrgCommands::Delete(_) => "delete",
                OrgCommands::Upgrade(_) => "upgrade",
                OrgCommands::Set(_) => "set",
                OrgCommands::Member(_) => "member",
            }
        }

        // Verify variant names match expectations
        let show = show::OrgShow {
            slug: None,
            format: None,
            org: None,
        };
        assert_eq!(check_variant(&OrgCommands::Show(show)), "show");

        let create = create::OrgCreate {
            name: "Test".to_string(),
            email: None,
        };
        assert_eq!(check_variant(&OrgCommands::Create(create)), "create");

        let set = set::OrgSet {
            slug: "test".to_string(),
            no_verify: false,
        };
        assert_eq!(check_variant(&OrgCommands::Set(set)), "set");
    }
}

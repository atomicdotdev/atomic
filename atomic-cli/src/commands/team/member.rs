//! The `team member` subcommand group for managing team members.
//!
//! This module implements the `atomic team member` command group, which
//! provides subcommands for listing, adding, updating, and removing
//! members of a team within an organization on the remote atomic-storage
//! server.
//!
//! # Usage
//!
//! ```text
//! atomic team member <COMMAND>
//!
//! Commands:
//!   list    List team members
//!   add     Add a member to the team
//!   update  Update a team member's role
//!   remove  Remove a member from the team
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all members of a team
//! $ atomic team member list backend-eng
//! IDENTITY_ID                           ROLE         ADDED
//! 550e8400-e29b-41d4-a716-446655440000  maintainer   2025-01-15T10:30:00Z
//! 6ba7b810-9dad-11d1-80b4-00c04fd430c8  member       2025-02-20T14:00:00Z
//!
//! # Add a member to a team
//! $ atomic team member add backend-eng 6ba7b810-9dad-11d1-80b4-00c04fd430c8
//! ✓ Added member to team: backend-eng
//!
//! # Update a team member's role
//! $ atomic team member update backend-eng 6ba7b810-9dad-11d1-80b4-00c04fd430c8 --role maintainer
//! ✓ Updated team member role to: maintainer
//!
//! # Remove a member from a team
//! $ atomic team member remove backend-eng 6ba7b810-9dad-11d1-80b4-00c04fd430c8
//! ```

use clap::{Parser, Subcommand};

use atomic_teams::types::TeamRole;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning, Column, KeyValueTable, Table};

/// Available team member subcommands.
#[derive(Subcommand, Debug)]
pub enum TeamMemberCommands {
    /// List team members.
    ///
    /// Shows all members of a team with their identity IDs, roles,
    /// and the date they were added.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team member list backend-eng
    /// atomic team member list backend-eng --org acme-corp
    /// atomic team member list backend-eng --format json
    /// ```
    List(TeamMemberList),

    /// Add a member to the team.
    ///
    /// Adds an identity to the team with the specified role. The
    /// identity must already be a member of the organization.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team member add backend-eng 550e8400-e29b-41d4-a716-446655440000
    /// atomic team member add backend-eng 550e8400-e29b-41d4-a716-446655440000 --role maintainer
    /// ```
    Add(TeamMemberAdd),

    /// Update a team member's role.
    ///
    /// Changes the role of an existing team member.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team member update backend-eng 550e8400-e29b-41d4-a716-446655440000 --role maintainer
    /// ```
    Update(TeamMemberUpdate),

    /// Remove a member from the team.
    ///
    /// Removes an identity from the team. The identity remains a
    /// member of the organization.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic team member remove backend-eng 550e8400-e29b-41d4-a716-446655440000
    /// atomic team member remove backend-eng 550e8400-e29b-41d4-a716-446655440000 --force
    /// ```
    Remove(TeamMemberRemove),
}

/// Manage team members.
///
/// Add, remove, and update member roles within a team. Members are
/// identified by their identity UUID, which can be found using
/// `atomic identity show` or `atomic org member list`.
#[derive(Debug, clap::Args)]
pub struct TeamMemberCmd {
    /// The team member subcommand to run.
    #[command(subcommand)]
    pub command: TeamMemberCommands,
}

impl Command for TeamMemberCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            TeamMemberCommands::List(cmd) => cmd.run(),
            TeamMemberCommands::Add(cmd) => cmd.run(),
            TeamMemberCommands::Update(cmd) => cmd.run(),
            TeamMemberCommands::Remove(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// List team members.
///
/// Displays all members of a team in a table showing their identity ID,
/// role, and the date they were added.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct TeamMemberList {
    /// Slug of the team whose members to list.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "TEAM_SLUG")]
    pub team_slug: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Output format.
    ///
    /// Use `table` for human-readable output or `json` for machine-readable.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
}

impl Command for TeamMemberList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamMemberList {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref()).await?;
        let org_slug = client.org_slug().to_string();

        let members =
            atomic_teams::team_member::list_team_members(&client, &org_slug, &self.team_slug)
                .await
                .map_err(|e| CliError::RemoteError {
                    message: e.to_string(),
                    url: None,
                })?;

        let is_json = self
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);

        if is_json {
            let json = serde_json::to_string_pretty(&members).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to serialize team members: {e}"))
            })?;
            println!("{json}");
        } else if members.is_empty() {
            print_hint(&format!("No members found in team '{}'.", self.team_slug));
            print_hint(&format!(
                "Use 'atomic team member add {} <identity-id>' to add one.",
                self.team_slug
            ));
        } else {
            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("IDENTITY_ID").min_width(36),
                Column::new("ROLE").min_width(12),
                Column::new("ADDED").min_width(20),
            ]);

            for member in &members {
                table.add_row(vec![
                    member.identity_id.to_string(),
                    member.role.to_string(),
                    member.added_at.to_rfc3339(),
                ]);
            }

            println!("{table}");

            print_hint(&format!(
                "{} member(s) in team '{}'",
                members.len(),
                self.team_slug
            ));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Add
// ---------------------------------------------------------------------------

/// Add a member to the team.
///
/// Adds an identity to the team with the specified role. The identity
/// must already be a member of the organization. The default role is
/// `contributor`.
#[derive(Debug, Parser)]
#[command(name = "add")]
pub struct TeamMemberAdd {
    /// Slug of the team to add the member to.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "TEAM_SLUG")]
    pub team_slug: String,

    /// Identity to add — email, name, or UUID.
    ///
    /// The identity must already be a member of the organization. You
    /// can specify an email address, display name, or raw UUID.
    #[arg(required = true, value_name = "IDENTITY")]
    pub identity: String,

    /// Role for the team member.
    ///
    /// Valid roles: `maintainer`, `contributor` (default), `collaborator`, `consumer`.
    #[arg(long, default_value = "contributor", value_name = "ROLE")]
    pub role: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for TeamMemberAdd {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamMemberAdd {
    async fn execute(&self) -> CliResult<()> {
        let role: TeamRole = self.role.parse().map_err(|_| CliError::InvalidArgument {
            message: format!(
                "Invalid role '{}'. Valid roles: member, maintainer",
                self.role
            ),
        })?;

        let client = build_client(self.org.as_deref()).await?;
        let org_slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        let member = atomic_teams::team_member::add_team_member(
            &client,
            &org_slug,
            &self.team_slug,
            identity_id,
            role,
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!("Added member to team: {}", self.team_slug));
        println!();

        let table = KeyValueTable::new()
            .add("Identity", member.identity_id.to_string())
            .add("Role", member.role.to_string())
            .add("Team", &self.team_slug)
            .add("Added", member.added_at.to_rfc3339());

        println!("{table}");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Update a team member's role.
///
/// Changes the role of an existing team member. You must have
/// maintainer permissions on the team, or admin/owner permissions
/// on the organization.
#[derive(Debug, Parser)]
#[command(name = "update")]
pub struct TeamMemberUpdate {
    /// Slug of the team containing the member.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "TEAM_SLUG")]
    pub team_slug: String,

    /// Identity to update — email, name, or UUID.
    ///
    /// The identity must be an existing member of the team.
    #[arg(required = true, value_name = "IDENTITY")]
    pub identity: String,

    /// New role for the team member.
    ///
    /// Valid roles: `maintainer`, `contributor`, `collaborator`, `consumer`.
    #[arg(long, required = true, value_name = "ROLE")]
    pub role: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for TeamMemberUpdate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamMemberUpdate {
    async fn execute(&self) -> CliResult<()> {
        let role: TeamRole = self.role.parse().map_err(|_| CliError::InvalidArgument {
            message: format!(
                "Invalid role '{}'. Valid roles: member, maintainer",
                self.role
            ),
        })?;

        let client = build_client(self.org.as_deref()).await?;
        let org_slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        let member = atomic_teams::team_member::update_team_member_role(
            &client,
            &org_slug,
            &self.team_slug,
            identity_id,
            role,
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!("Updated team member role to: {}", member.role));
        println!();

        let table = KeyValueTable::new()
            .add("Identity", member.identity_id.to_string())
            .add("Role", member.role.to_string())
            .add("Team", &self.team_slug);

        println!("{table}");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

/// Remove a member from the team.
///
/// Removes an identity from the team. The identity remains a member
/// of the organization — only the team membership is revoked. A
/// confirmation prompt is shown unless `--force` is passed.
#[derive(Debug, Parser)]
#[command(name = "remove")]
pub struct TeamMemberRemove {
    /// Slug of the team to remove the member from.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "TEAM_SLUG")]
    pub team_slug: String,

    /// Identity to remove — email, name, or UUID.
    ///
    /// The identity must be an existing member of the team.
    #[arg(required = true, value_name = "IDENTITY")]
    pub identity: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Skip the confirmation prompt.
    ///
    /// By default, the command asks for confirmation before removing
    /// a member. Use this flag in scripts or when you are certain.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Command for TeamMemberRemove {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamMemberRemove {
    async fn execute(&self) -> CliResult<()> {
        // Prompt for confirmation unless --force is set.
        if !self.force {
            print_warning(&format!(
                "This will remove member '{}' from team '{}'.",
                self.identity, self.team_slug
            ));

            let confirmed = dialoguer::Confirm::new()
                .with_prompt("Are you sure?")
                .default(false)
                .interact()
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to read confirmation: {e}"))
                })?;

            if !confirmed {
                return Err(CliError::Cancelled);
            }
        }

        let client = build_client(self.org.as_deref()).await?;
        let org_slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        atomic_teams::team_member::remove_team_member(
            &client,
            &org_slug,
            &self.team_slug,
            identity_id,
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!(
            "Removed member {} from team: {}",
            identity_id, self.team_slug
        ));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- TeamMemberList --

    #[test]
    fn list_required_team_slug() {
        let cmd = TeamMemberList {
            team_slug: "backend-eng".to_string(),
            org: None,
            format: None,
        };
        assert_eq!(cmd.team_slug, "backend-eng");
        assert!(cmd.org.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn list_with_org_override() {
        let cmd = TeamMemberList {
            team_slug: "backend-eng".to_string(),
            org: Some("acme".to_string()),
            format: None,
        };
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn list_json_format() {
        let cmd = TeamMemberList {
            team_slug: "backend-eng".to_string(),
            org: None,
            format: Some("json".to_string()),
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(is_json);
    }

    #[test]
    fn list_table_format_is_default() {
        let cmd = TeamMemberList {
            team_slug: "backend-eng".to_string(),
            org: None,
            format: None,
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(!is_json);
    }

    // -- TeamMemberAdd --

    #[test]
    fn add_default_role() {
        let cmd = TeamMemberAdd {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "contributor".to_string(),
            org: None,
        };
        assert_eq!(cmd.team_slug, "backend-eng");
        assert_eq!(cmd.role, "contributor");
    }

    #[test]
    fn add_with_maintainer_role() {
        let cmd = TeamMemberAdd {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "maintainer".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.role, "maintainer");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn add_accepts_email() {
        let cmd = TeamMemberAdd {
            team_slug: "backend-eng".to_string(),
            identity: "alice@example.com".to_string(),
            role: "contributor".to_string(),
            org: None,
        };
        assert!(cmd.identity.contains('@'));
    }

    #[test]
    fn add_accepts_name() {
        let cmd = TeamMemberAdd {
            team_slug: "backend-eng".to_string(),
            identity: "alice".to_string(),
            role: "contributor".to_string(),
            org: None,
        };
        assert!(!cmd.identity.contains('@'));
        assert!(uuid::Uuid::parse_str(&cmd.identity).is_err());
    }

    // -- TeamMemberUpdate --

    #[test]
    fn update_fields() {
        let cmd = TeamMemberUpdate {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "maintainer".to_string(),
            org: None,
        };
        assert_eq!(cmd.team_slug, "backend-eng");
        assert_eq!(cmd.role, "maintainer");
    }

    #[test]
    fn update_with_org() {
        let cmd = TeamMemberUpdate {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "contributor".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    // -- TeamMemberRemove --

    #[test]
    fn remove_default_no_force() {
        let cmd = TeamMemberRemove {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: false,
        };
        assert!(!cmd.force);
    }

    #[test]
    fn remove_with_force() {
        let cmd = TeamMemberRemove {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: true,
        };
        assert!(cmd.force);
    }

    #[test]
    fn remove_with_org_override() {
        let cmd = TeamMemberRemove {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: Some("other".to_string()),
            force: true,
        };
        assert_eq!(cmd.org.as_deref(), Some("other"));
    }

    // -- Role parsing --

    #[test]
    fn parse_valid_team_roles() {
        assert!("maintainer".parse::<TeamRole>().is_ok());
        assert!("contributor".parse::<TeamRole>().is_ok());
        assert!("collaborator".parse::<TeamRole>().is_ok());
        assert!("consumer".parse::<TeamRole>().is_ok());
    }

    #[test]
    fn parse_invalid_team_role() {
        assert!("member".parse::<TeamRole>().is_err());
        assert!("admin".parse::<TeamRole>().is_err());
        assert!("owner".parse::<TeamRole>().is_err());
    }

    // -- TeamMemberCommands dispatch --

    #[test]
    fn command_variants() {
        fn check_variant(cmd: &TeamMemberCommands) -> &'static str {
            match cmd {
                TeamMemberCommands::List(_) => "list",
                TeamMemberCommands::Add(_) => "add",
                TeamMemberCommands::Update(_) => "update",
                TeamMemberCommands::Remove(_) => "remove",
            }
        }

        let list = TeamMemberList {
            team_slug: "backend-eng".to_string(),
            org: None,
            format: None,
        };
        assert_eq!(check_variant(&TeamMemberCommands::List(list)), "list");

        let add = TeamMemberAdd {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "contributor".to_string(),
            org: None,
        };
        assert_eq!(check_variant(&TeamMemberCommands::Add(add)), "add");

        let update = TeamMemberUpdate {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "maintainer".to_string(),
            org: None,
        };
        assert_eq!(check_variant(&TeamMemberCommands::Update(update)), "update");

        let remove = TeamMemberRemove {
            team_slug: "backend-eng".to_string(),
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: false,
        };
        assert_eq!(check_variant(&TeamMemberCommands::Remove(remove)), "remove");
    }
}

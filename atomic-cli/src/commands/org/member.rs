//! The `org member` subcommand group for managing organization members.
//!
//! This module implements the `atomic org member` command group, which
//! provides subcommands for listing, adding, updating, and removing
//! members of an organization on the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic org member <COMMAND>
//!
//! Commands:
//!   list    List organization members
//!   add     Add a member to the organization
//!   update  Update a member's role
//!   remove  Remove a member from the organization
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all members of the current org
//! $ atomic org member list
//! IDENTITY_ID                           ROLE     JOINED
//! 550e8400-e29b-41d4-a716-446655440000  owner    2025-01-15T10:30:00Z
//! 6ba7b810-9dad-11d1-80b4-00c04fd430c8  admin    2025-02-20T14:00:00Z
//!
//! # Add a member
//! $ atomic org member add 6ba7b810-9dad-11d1-80b4-00c04fd430c8 --role admin
//! ✓ Added member to organization
//!
//! # Update a member's role
//! $ atomic org member update 6ba7b810-9dad-11d1-80b4-00c04fd430c8 --role member
//! ✓ Updated member role to: member
//!
//! # Remove a member
//! $ atomic org member remove 6ba7b810-9dad-11d1-80b4-00c04fd430c8
//! ```

use clap::{Parser, Subcommand};

use atomic_teams::types::OrgRole;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning, Column, KeyValueTable, Table};

/// Available member subcommands.
#[derive(Subcommand, Debug)]
pub enum MemberCommands {
    /// List organization members.
    ///
    /// Shows all members of the organization with their identity IDs,
    /// roles, and join dates.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org member list
    /// atomic org member list --org acme-corp
    /// atomic org member list --format json
    /// ```
    List(MemberList),

    /// Add a member to the organization.
    ///
    /// Adds an identity to the organization with the specified role.
    /// The identity must already be registered on the server.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org member add 550e8400-e29b-41d4-a716-446655440000
    /// atomic org member add 550e8400-e29b-41d4-a716-446655440000 --role admin
    /// ```
    Add(MemberAdd),

    /// Update a member's role.
    ///
    /// Changes the role of an existing organization member.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org member update 550e8400-e29b-41d4-a716-446655440000 --role admin
    /// ```
    Update(MemberUpdate),

    /// Remove a member from the organization.
    ///
    /// Removes an identity from the organization. The last owner
    /// cannot be removed.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic org member remove 550e8400-e29b-41d4-a716-446655440000
    /// atomic org member remove 550e8400-e29b-41d4-a716-446655440000 --force
    /// ```
    Remove(MemberRemove),
}

/// Manage organization members.
///
/// Add, remove, and update member roles within an organization.
/// Members are identified by their identity UUID, which can be found
/// using `atomic identity show`.
#[derive(Debug, clap::Args)]
pub struct MemberCmd {
    /// The member subcommand to run.
    #[command(subcommand)]
    pub command: MemberCommands,
}

impl Command for MemberCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            MemberCommands::List(cmd) => cmd.run(),
            MemberCommands::Add(cmd) => cmd.run(),
            MemberCommands::Update(cmd) => cmd.run(),
            MemberCommands::Remove(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

/// List organization members.
///
/// Displays all members of the organization in a table showing their
/// identity ID, role, and join date.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct MemberList {
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

impl Command for MemberList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl MemberList {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;
        let slug = client.org_slug().to_string();

        let members = atomic_teams::member::list_members(&client, &slug)
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
                CliError::Internal(anyhow::anyhow!("Failed to serialize members: {e}"))
            })?;
            println!("{json}");
        } else if members.is_empty() {
            print_hint("No members found in this organization.");
        } else {
            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("IDENTITY_ID").min_width(36),
                Column::new("ROLE").min_width(8),
                Column::new("JOINED").min_width(20),
            ]);

            for member in &members {
                table.add_row(vec![
                    member.identity_id.to_string(),
                    member.role.to_string(),
                    member.joined_at.to_rfc3339(),
                ]);
            }

            println!("{table}");

            print_hint(&format!("{} member(s) total", members.len()));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Add
// ---------------------------------------------------------------------------

/// Add a member to the organization.
///
/// Adds an identity to the organization with the specified role. The
/// identity must already be registered on the remote server. The default
/// role is `member`.
#[derive(Debug, Parser)]
#[command(name = "add")]
pub struct MemberAdd {
    /// Identity to add — email, name, or UUID.
    ///
    /// The identity must already be registered on the server. You can
    /// pass a UUID directly, an email address, or the identity's
    /// display name and it will be resolved automatically.
    #[arg(required = true, value_name = "IDENTITY")]
    pub identity: String,

    /// Role to assign to the new member.
    ///
    /// Valid roles: `owner`, `admin`, `member`. Defaults to `member`.
    #[arg(long, default_value = "member", value_name = "ROLE")]
    pub role: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for MemberAdd {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl MemberAdd {
    async fn execute(&self) -> CliResult<()> {
        let role: OrgRole = self.role.parse().map_err(|_| CliError::InvalidArgument {
            message: format!(
                "Invalid role '{}'. Valid roles: owner, admin, member",
                self.role
            ),
        })?;

        let client = build_client(self.org.as_deref())?;
        let slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        let member = atomic_teams::member::add_member(&client, &slug, identity_id, role)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success("Added member to organization");
        println!();

        let table = KeyValueTable::new()
            .add("Identity", &member.identity_id.to_string())
            .add("Role", &member.role.to_string())
            .add("Organization", &slug)
            .add("Joined", &member.joined_at.to_rfc3339());

        println!("{table}");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

/// Update a member's role.
///
/// Changes the role of an existing organization member. You must have
/// sufficient permissions (admin or owner) to modify member roles.
#[derive(Debug, Parser)]
#[command(name = "update")]
pub struct MemberUpdate {
    /// Identity to update — email, name, or UUID.
    ///
    /// The identity must be an existing member of the organization.
    #[arg(required = true, value_name = "IDENTITY")]
    pub identity: String,

    /// New role for the member.
    ///
    /// Valid roles: `owner`, `admin`, `member`.
    #[arg(long, required = true, value_name = "ROLE")]
    pub role: String,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for MemberUpdate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl MemberUpdate {
    async fn execute(&self) -> CliResult<()> {
        let role: OrgRole = self.role.parse().map_err(|_| CliError::InvalidArgument {
            message: format!(
                "Invalid role '{}'. Valid roles: owner, admin, member",
                self.role
            ),
        })?;

        let client = build_client(self.org.as_deref())?;
        let slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        let member = atomic_teams::member::update_member_role(&client, &slug, identity_id, role)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Updated member role to: {}", member.role));
        println!();

        let table = KeyValueTable::new()
            .add("Identity", &member.identity_id.to_string())
            .add("Role", &member.role.to_string())
            .add("Organization", &slug);

        println!("{table}");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

/// Remove a member from the organization.
///
/// Removes an identity from the organization. The last owner of an
/// organization cannot be removed. A confirmation prompt is shown
/// unless `--force` is passed.
#[derive(Debug, Parser)]
#[command(name = "remove")]
pub struct MemberRemove {
    /// Identity to remove — email, name, or UUID.
    ///
    /// The identity must be an existing member of the organization.
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

impl Command for MemberRemove {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl MemberRemove {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;
        let slug = client.org_slug().to_string();

        let identity_id =
            crate::commands::client::resolve_identity(&client, &self.identity).await?;

        // Prompt for confirmation unless --force is set.
        if !self.force {
            print_warning(&format!(
                "This will remove member '{}' from the organization.",
                identity_id
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

        atomic_teams::member::remove_member(&client, &slug, identity_id)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Removed member: {}", identity_id));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- MemberList --

    #[test]
    fn list_default_args() {
        let cmd = MemberList {
            org: None,
            format: None,
        };
        assert!(cmd.org.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn list_with_org_override() {
        let cmd = MemberList {
            org: Some("acme".to_string()),
            format: None,
        };
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn list_json_format() {
        let cmd = MemberList {
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

    // -- MemberAdd --

    #[test]
    fn add_default_role() {
        let cmd = MemberAdd {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "member".to_string(),
            org: None,
        };
        assert_eq!(cmd.role, "member");
    }

    #[test]
    fn add_with_admin_role() {
        let cmd = MemberAdd {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "admin".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.role, "admin");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn add_accepts_email() {
        let cmd = MemberAdd {
            identity: "alice@example.com".to_string(),
            role: "member".to_string(),
            org: None,
        };
        assert!(cmd.identity.contains('@'));
    }

    #[test]
    fn add_accepts_name() {
        let cmd = MemberAdd {
            identity: "alice".to_string(),
            role: "member".to_string(),
            org: None,
        };
        assert!(!cmd.identity.contains('@'));
        assert!(uuid::Uuid::parse_str(&cmd.identity).is_err());
    }

    #[test]
    fn add_invalid_uuid_detected() {
        let result = uuid::Uuid::parse_str("not-a-uuid");
        assert!(result.is_err());
    }

    #[test]
    fn add_valid_uuid_parsed() {
        let result = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000");
        assert!(result.is_ok());
    }

    // -- MemberUpdate --

    #[test]
    fn update_fields() {
        let cmd = MemberUpdate {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "owner".to_string(),
            org: None,
        };
        assert_eq!(cmd.role, "owner");
    }

    #[test]
    fn update_with_org() {
        let cmd = MemberUpdate {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "admin".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    // -- MemberRemove --

    #[test]
    fn remove_default_no_force() {
        let cmd = MemberRemove {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: false,
        };
        assert!(!cmd.force);
    }

    #[test]
    fn remove_with_force() {
        let cmd = MemberRemove {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: true,
        };
        assert!(cmd.force);
    }

    #[test]
    fn remove_with_org_override() {
        let cmd = MemberRemove {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: Some("other".to_string()),
            force: true,
        };
        assert_eq!(cmd.org.as_deref(), Some("other"));
    }

    // -- Role parsing --

    #[test]
    fn parse_valid_roles() {
        assert!("owner".parse::<OrgRole>().is_ok());
        assert!("admin".parse::<OrgRole>().is_ok());
        assert!("member".parse::<OrgRole>().is_ok());
    }

    #[test]
    fn parse_invalid_role() {
        assert!("superuser".parse::<OrgRole>().is_err());
    }

    // -- MemberCommands dispatch --

    #[test]
    fn command_variants() {
        fn check_variant(cmd: &MemberCommands) -> &'static str {
            match cmd {
                MemberCommands::List(_) => "list",
                MemberCommands::Add(_) => "add",
                MemberCommands::Update(_) => "update",
                MemberCommands::Remove(_) => "remove",
            }
        }

        let list = MemberList {
            org: None,
            format: None,
        };
        assert_eq!(check_variant(&MemberCommands::List(list)), "list");

        let add = MemberAdd {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "member".to_string(),
            org: None,
        };
        assert_eq!(check_variant(&MemberCommands::Add(add)), "add");

        let update = MemberUpdate {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            role: "admin".to_string(),
            org: None,
        };
        assert_eq!(check_variant(&MemberCommands::Update(update)), "update");

        let remove = MemberRemove {
            identity: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            org: None,
            force: false,
        };
        assert_eq!(check_variant(&MemberCommands::Remove(remove)), "remove");
    }
}

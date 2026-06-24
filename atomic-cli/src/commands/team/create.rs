//! The `team create` command for creating a new team in an organization.
//!
//! This module implements the `atomic team create` command, which creates
//! a new team within the current organization on the remote atomic-storage
//! server.
//!
//! # Usage
//!
//! ```text
//! atomic team create [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Display name for the new team
//!
//! Options:
//!   --description <DESC>        Optional description for the team
//!   --visibility <VISIBILITY>   Team visibility: visible or secret [default: visible]
//!   --org <ORG>                 Organization slug override
//!   -h, --help                  Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Create a basic team
//! $ atomic team create "Backend Engineering"
//! ✓ Created team: backend-engineering
//!
//!   Name:        Backend Engineering
//!   Slug:        backend-engineering
//!   Visibility:  visible
//!
//! Next steps:
//!   atomic team member add backend-engineering <id>  Add members to the team
//!   atomic team show backend-engineering             View team details
//!
//! # Create a secret team with a description
//! $ atomic team create "Secret Ops" --visibility secret --description "Top secret operations"
//!
//! # Create a team in a specific org
//! $ atomic team create "Frontend" --org acme-corp
//! ```

use clap::Parser;

use atomic_teams::types::TeamVisibility;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_next_steps, print_success, KeyValueTable};

/// Create a new team.
///
/// Creates a new team within the current organization on the remote
/// server. The URL-safe slug is derived server-side from the display
/// name. You must have admin or owner permissions on the organization.
#[derive(Debug, Parser)]
#[command(name = "create")]
pub struct TeamCreate {
    /// Display name for the team.
    ///
    /// The server derives a URL-safe slug from this name (e.g.
    /// "Backend Engineering" → "backend-engineering"). The name can
    /// contain spaces and special characters.
    #[arg(required = true, value_name = "NAME")]
    pub name: String,

    /// Optional description for the team.
    ///
    /// A short description of the team's purpose, displayed on the
    /// team profile and in team listings.
    #[arg(long, value_name = "DESC")]
    pub description: Option<String>,

    /// Team visibility within the organization.
    ///
    /// `visible` — discoverable by all org members.
    /// `secret` — only visible to team members and org admins.
    ///
    /// Defaults to `visible` if not specified.
    #[arg(long, value_name = "VISIBILITY")]
    pub visibility: Option<String>,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for TeamCreate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamCreate {
    async fn execute(&self) -> CliResult<()> {
        // Parse visibility if provided.
        let visibility = self
            .visibility
            .as_deref()
            .map(|v| {
                v.parse::<TeamVisibility>()
                    .map_err(|_| CliError::InvalidArgument {
                        message: format!(
                            "Invalid visibility '{}'. Valid values: visible, secret",
                            v
                        ),
                    })
            })
            .transpose()?;

        let client = build_client(self.org.as_deref(), None).await?;
        let org_slug = client.org_slug().to_string();

        let info = atomic_teams::team::create_team(
            &client,
            &org_slug,
            &self.name,
            self.description.as_deref(),
            visibility,
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!("Created team: {}", info.slug));
        println!();

        let table = KeyValueTable::new()
            .add("Name", &info.name)
            .add("Slug", &info.slug)
            .add("Visibility", info.visibility.to_string());
        let table = if let Some(desc) = &info.description {
            table.add("Description", desc)
        } else {
            table
        };
        let table = table
            .add("Organization", &org_slug)
            .add("ID", info.id.to_string());

        println!("{table}");

        print_next_steps(&[
            (
                &format!("atomic team member add {} <id>", info.slug),
                "Add members to the team",
            ),
            (
                &format!("atomic team show {}", info.slug),
                "View team details",
            ),
        ]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_name() {
        let cmd = TeamCreate {
            name: "Backend Engineering".to_string(),
            description: None,
            visibility: None,
            org: None,
        };
        assert_eq!(cmd.name, "Backend Engineering");
        assert!(cmd.description.is_none());
        assert!(cmd.visibility.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_all_options() {
        let cmd = TeamCreate {
            name: "Secret Ops".to_string(),
            description: Some("Top secret operations".to_string()),
            visibility: Some("secret".to_string()),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.name, "Secret Ops");
        assert_eq!(cmd.description.as_deref(), Some("Top secret operations"));
        assert_eq!(cmd.visibility.as_deref(), Some("secret"));
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn visibility_parsing_valid() {
        assert!("visible".parse::<TeamVisibility>().is_ok());
        assert!("secret".parse::<TeamVisibility>().is_ok());
    }

    #[test]
    fn visibility_parsing_invalid() {
        assert!("private".parse::<TeamVisibility>().is_err());
        assert!("public".parse::<TeamVisibility>().is_err());
    }

    #[test]
    fn name_with_special_characters() {
        let cmd = TeamCreate {
            name: "R&D / Platform".to_string(),
            description: None,
            visibility: None,
            org: None,
        };
        assert_eq!(cmd.name, "R&D / Platform");
    }

    #[test]
    fn default_visibility_is_none() {
        let cmd = TeamCreate {
            name: "Frontend".to_string(),
            description: None,
            visibility: None,
            org: None,
        };
        // When visibility is None, the server applies its default (visible).
        assert!(cmd.visibility.is_none());
    }

    #[test]
    fn with_org_override() {
        let cmd = TeamCreate {
            name: "Infra".to_string(),
            description: None,
            visibility: None,
            org: Some("other-org".to_string()),
        };
        assert_eq!(cmd.org.as_deref(), Some("other-org"));
    }
}

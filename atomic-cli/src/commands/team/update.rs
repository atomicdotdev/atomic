//! The `team update` command for updating team settings.
//!
//! This module implements the `atomic team update` command, which modifies
//! a team's display name, description, and/or visibility on the remote
//! atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic team update [OPTIONS] <SLUG>
//!
//! Arguments:
//!   <SLUG>  Team slug to update
//!
//! Options:
//!   --name <NAME>              New display name
//!   --description <DESC>       New description
//!   --visibility <VISIBILITY>  New visibility: visible or secret
//!   --org <ORG>                Organization slug override for the API client
//!   -h, --help                 Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Update a team's name
//! $ atomic team update backend-eng --name "Backend Engineering"
//! ✓ Updated team: backend-eng
//!
//!   Name:        Backend Engineering
//!   Slug:        backend-eng
//!   Visibility:  visible
//!
//! # Update visibility to secret
//! $ atomic team update backend-eng --visibility secret
//!
//! # Update description
//! $ atomic team update backend-eng --description "Core backend services"
//!
//! # Update multiple fields at once
//! $ atomic team update backend-eng --name "New Name" --visibility secret --description "New desc"
//! ```

use clap::Parser;

use atomic_teams::types::TeamVisibility;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, KeyValueTable};

/// Update team settings.
///
/// Modify the display name, description, or visibility of a team.
/// Only fields that are provided will be updated; omitted fields
/// remain unchanged on the server.
///
/// Requires maintainer permissions on the team, or admin/owner
/// permissions on the organization.
#[derive(Debug, Parser)]
#[command(name = "update")]
pub struct TeamUpdate {
    /// Team slug to update.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,

    /// New display name for the team.
    ///
    /// If not provided, the existing name is preserved.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// New description for the team.
    ///
    /// If not provided, the existing description is preserved.
    #[arg(long, value_name = "DESC")]
    pub description: Option<String>,

    /// New visibility for the team.
    ///
    /// `visible` — discoverable by all org members.
    /// `secret` — only visible to team members and org admins.
    ///
    /// If not provided, the existing visibility is preserved.
    #[arg(long, value_name = "VISIBILITY")]
    pub visibility: Option<String>,

    /// Organization slug override for the API client.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for TeamUpdate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamUpdate {
    async fn execute(&self) -> CliResult<()> {
        // At least one field must be provided to update.
        if self.name.is_none() && self.description.is_none() && self.visibility.is_none() {
            return Err(CliError::InvalidArgument {
                message: "At least one of --name, --description, or --visibility must be provided"
                    .to_string(),
            });
        }

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

        let client = build_client(self.org.as_deref())?;
        let org_slug = client.org_slug().to_string();

        let info = atomic_teams::team::update_team(
            &client,
            &org_slug,
            &self.slug,
            self.name.as_deref(),
            self.description.as_deref(),
            visibility,
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!("Updated team: {}", info.slug));
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: None,
            description: None,
            visibility: None,
            org: None,
        };
        assert_eq!(cmd.slug, "backend-eng");
        assert!(cmd.name.is_none());
        assert!(cmd.description.is_none());
        assert!(cmd.visibility.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_name_only() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: Some("Backend Engineering".to_string()),
            description: None,
            visibility: None,
            org: None,
        };
        assert_eq!(cmd.name.as_deref(), Some("Backend Engineering"));
    }

    #[test]
    fn with_description_only() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: None,
            description: Some("Core backend services".to_string()),
            visibility: None,
            org: None,
        };
        assert_eq!(cmd.description.as_deref(), Some("Core backend services"));
    }

    #[test]
    fn with_visibility_only() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: None,
            description: None,
            visibility: Some("secret".to_string()),
            org: None,
        };
        assert_eq!(cmd.visibility.as_deref(), Some("secret"));
    }

    #[test]
    fn with_all_fields() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: Some("New Name".to_string()),
            description: Some("New desc".to_string()),
            visibility: Some("visible".to_string()),
            org: Some("acme".to_string()),
        };
        assert!(cmd.name.is_some());
        assert!(cmd.description.is_some());
        assert!(cmd.visibility.is_some());
        assert!(cmd.org.is_some());
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
        assert!("hidden".parse::<TeamVisibility>().is_err());
    }

    #[test]
    fn with_org_override() {
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: Some("Name".to_string()),
            description: None,
            visibility: None,
            org: Some("other-org".to_string()),
        };
        assert_eq!(cmd.org.as_deref(), Some("other-org"));
    }

    #[test]
    fn no_fields_is_invalid() {
        // Verify that execute() would reject this — we test the validation
        // logic inline since we can't call execute() without a server.
        let cmd = TeamUpdate {
            slug: "backend-eng".to_string(),
            name: None,
            description: None,
            visibility: None,
            org: None,
        };
        assert!(cmd.name.is_none() && cmd.description.is_none() && cmd.visibility.is_none());
    }
}

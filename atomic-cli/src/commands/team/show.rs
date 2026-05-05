//! The `team show` command for displaying team details.
//!
//! This module implements the `atomic team show` command, which fetches
//! and displays metadata for a specific team from the remote server.
//!
//! # Usage
//!
//! ```text
//! atomic team show [OPTIONS] <SLUG>
//!
//! Arguments:
//!   <SLUG>  Team slug to display
//!
//! Options:
//!   --org <ORG>        Organization slug override
//!   --format <FORMAT>  Output format: table or json [default: table]
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Show team details
//! $ atomic team show backend-engineering
//!   Name:        Backend Engineering
//!   Slug:        backend-engineering
//!   Visibility:  visible
//!   Description: Core backend services team
//!   Organization: acme-corp
//!   ID:          550e8400-e29b-41d4-a716-446655440000
//!   Created:     2025-01-15T10:30:00Z
//!   Updated:     2025-02-20T14:00:00Z
//!
//! # Show in JSON format
//! $ atomic team show backend-engineering --format json
//!
//! # Show a team in a specific org
//! $ atomic team show backend-engineering --org acme-corp
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::KeyValueTable;

/// Show team details.
///
/// Fetches metadata for a specific team from the remote server and
/// displays it in a human-readable table or JSON format. The team
/// must be visible to the caller (either `visible` teams or `secret`
/// teams where the caller is a member or org admin).
#[derive(Debug, Parser)]
#[command(name = "show")]
pub struct TeamShow {
    /// Team slug to display.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,

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

impl Command for TeamShow {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamShow {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;
        let org_slug = client.org_slug().to_string();

        let info = atomic_teams::team::get_team(&client, &org_slug, &self.slug)
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
            let json = serde_json::to_string_pretty(&info).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to serialize team info: {e}"))
            })?;
            println!("{json}");
        } else {
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
                .add("ID", info.id.to_string())
                .add("Created", info.created_at.to_rfc3339())
                .add("Updated", info.updated_at.to_rfc3339());

            println!("{table}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = TeamShow {
            slug: "backend-engineering".to_string(),
            org: None,
            format: None,
        };
        assert_eq!(cmd.slug, "backend-engineering");
        assert!(cmd.org.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn with_org_override() {
        let cmd = TeamShow {
            slug: "backend-eng".to_string(),
            org: Some("acme-corp".to_string()),
            format: None,
        };
        assert_eq!(cmd.slug, "backend-eng");
        assert_eq!(cmd.org.as_deref(), Some("acme-corp"));
    }

    #[test]
    fn json_format_flag() {
        let cmd = TeamShow {
            slug: "backend-eng".to_string(),
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
    fn table_format_is_default() {
        let cmd = TeamShow {
            slug: "backend-eng".to_string(),
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

    #[test]
    fn json_format_case_insensitive() {
        for variant in &["json", "JSON", "Json"] {
            let cmd = TeamShow {
                slug: "test".to_string(),
                org: None,
                format: Some(variant.to_string()),
            };
            let is_json = cmd
                .format
                .as_deref()
                .map(|f| f.eq_ignore_ascii_case("json"))
                .unwrap_or(false);
            assert!(is_json, "Expected true for format={variant}");
        }
    }

    #[test]
    fn slug_with_hyphens() {
        let cmd = TeamShow {
            slug: "my-cool-team-123".to_string(),
            org: None,
            format: None,
        };
        assert_eq!(cmd.slug, "my-cool-team-123");
    }

    #[test]
    fn all_options_combined() {
        let cmd = TeamShow {
            slug: "backend-eng".to_string(),
            org: Some("acme".to_string()),
            format: Some("json".to_string()),
        };
        assert_eq!(cmd.slug, "backend-eng");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert_eq!(cmd.format.as_deref(), Some("json"));
    }
}

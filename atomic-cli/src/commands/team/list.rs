//! The `team list` command for listing teams in an organization.
//!
//! This module implements the `atomic team list` command, which fetches
//! and displays all teams visible to the caller within the current
//! organization from the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic team list [OPTIONS]
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
//! # List all teams in the current org
//! $ atomic team list
//! NAME                  SLUG                  VISIBILITY  CREATED
//! Backend Engineering   backend-engineering   visible     2025-01-15T10:30:00Z
//! Secret Ops            secret-ops            secret      2025-02-20T14:00:00Z
//!
//! # List teams in a specific org
//! $ atomic team list --org acme-corp
//!
//! # JSON output
//! $ atomic team list --format json
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, Column, Table};

/// List teams in the organization.
///
/// Displays all teams visible to the caller within the current organization
/// in a table showing their name, slug, visibility, and creation date.
/// Secret teams are only returned when the caller is a member or an org
/// admin.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct TeamList {
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

impl Command for TeamList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamList {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref()).await?;
        let slug = client.org_slug().to_string();

        let teams = atomic_teams::team::list_teams(&client, &slug)
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
            let json = serde_json::to_string_pretty(&teams).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to serialize teams: {e}"))
            })?;
            println!("{json}");
        } else if teams.is_empty() {
            print_hint("No teams found in this organization.");
            print_hint("Use 'atomic team create <name>' to create one.");
        } else {
            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("NAME").min_width(20),
                Column::new("SLUG").min_width(20),
                Column::new("VISIBILITY").min_width(10),
                Column::new("CREATED").min_width(20),
            ]);

            for team in &teams {
                table.add_row(vec![
                    team.name.clone(),
                    team.slug.clone(),
                    team.visibility.to_string(),
                    team.created_at.to_rfc3339(),
                ]);
            }

            println!("{table}");

            print_hint(&format!("{} team(s) total", teams.len()));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = TeamList {
            org: None,
            format: None,
        };
        assert!(cmd.org.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn with_org_override() {
        let cmd = TeamList {
            org: Some("acme-corp".to_string()),
            format: None,
        };
        assert_eq!(cmd.org.as_deref(), Some("acme-corp"));
    }

    #[test]
    fn json_format() {
        let cmd = TeamList {
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
        let cmd = TeamList {
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
            let cmd = TeamList {
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
    fn table_format_explicit() {
        let cmd = TeamList {
            org: None,
            format: Some("table".to_string()),
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(!is_json);
    }
}

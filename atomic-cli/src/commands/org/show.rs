//! The `org show` command for displaying organization details.
//!
//! This module implements the `atomic org show` command, which fetches
//! and displays metadata for an organization from the remote server.
//!
//! # Usage
//!
//! ```text
//! atomic org show [OPTIONS] [SLUG]
//!
//! Arguments:
//!   [SLUG]  Organization slug (defaults to current org from config)
//!
//! Options:
//!   --format <FORMAT>  Output format: table or json [default: table]
//!   --org <ORG>        Organization slug override
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Show current default org
//! $ atomic org show
//!   Name:       Acme Corp
//!   Slug:       acme-corp
//!   Plan:       team
//!   Email:      admin@acme.com
//!   Created:    2025-01-15T10:30:00Z
//!
//! # Show a specific org
//! $ atomic org show acme-corp
//!
//! # JSON output
//! $ atomic org show --format json
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::KeyValueTable;

/// Show organization details.
///
/// Fetches metadata for an organization from the remote server and
/// displays it in a human-readable table or JSON format.
///
/// If no slug is provided, the current default organization from
/// global configuration is used.
#[derive(Debug, Parser)]
#[command(name = "show")]
pub struct OrgShow {
    /// Organization slug to display.
    ///
    /// If omitted, the current default organization from global
    /// configuration is used.
    #[arg(value_name = "SLUG")]
    pub slug: Option<String>,

    /// Output format.
    ///
    /// Use `table` for human-readable output or `json` for machine-readable.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// Organization slug override.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for OrgShow {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgShow {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;

        // Determine which slug to look up: explicit arg > client's org slug
        let slug = self.slug.as_deref().unwrap_or_else(|| client.org_slug());

        let info = atomic_teams::org::get_org(&client, slug)
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
                CliError::Internal(anyhow::anyhow!("Failed to serialize org info: {e}"))
            })?;
            println!("{json}");
        } else {
            let table = KeyValueTable::new()
                .add("Name", &info.name)
                .add("Slug", &info.slug)
                .add("Kind", &info.kind)
                .add("Plan", &info.plan);

            let table = if let Some(email) = &info.email {
                table.add("Email", email)
            } else {
                table
            };

            let table = table
                .add("ID", &info.id.to_string())
                .add("Created", &info.created_at.to_rfc3339())
                .add("Updated", &info.updated_at.to_rfc3339());

            println!("{table}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = OrgShow {
            slug: None,
            format: None,
            org: None,
        };
        assert!(cmd.slug.is_none());
        assert!(cmd.format.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn explicit_slug() {
        let cmd = OrgShow {
            slug: Some("acme".to_string()),
            format: None,
            org: None,
        };
        assert_eq!(cmd.slug.as_deref(), Some("acme"));
    }

    #[test]
    fn json_format_flag() {
        let cmd = OrgShow {
            slug: None,
            format: Some("json".to_string()),
            org: None,
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
        let cmd = OrgShow {
            slug: None,
            format: None,
            org: None,
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(!is_json);
    }

    #[test]
    fn org_override() {
        let cmd = OrgShow {
            slug: None,
            format: None,
            org: Some("other-org".to_string()),
        };
        assert_eq!(cmd.org.as_deref(), Some("other-org"));
    }
}

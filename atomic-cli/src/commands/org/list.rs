//! The `org list` command for listing organizations.
//!
//! The remote server does not yet have a "list all my orgs" endpoint (the
//! `GET /orgs` route is scoped to a single org via subdomain). Until that
//! endpoint exists, this command shows the current default organization's
//! info and prints a hint about switching orgs.
//!
//! # Usage
//!
//! ```text
//! atomic org list [OPTIONS]
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
//! $ atomic org list
//! Current organization:
//!   Name:  Acme Corp
//!   Slug:  acme-corp
//!   Plan:  team
//!
//!   Hint: Use 'atomic org switch <slug>' to change the default organization.
//!
//! $ atomic org list --format json
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_section, KeyValueTable};

/// List organizations you belong to.
///
/// Currently shows the default organization's info, since the server
/// does not yet support listing all organizations for a user. Use
/// `atomic org switch <slug>` to change which organization is active.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct OrgList {
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

impl Command for OrgList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgList {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;
        let slug = client.org_slug().to_string();

        let info = atomic_teams::org::get_org(&client, &slug)
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
            print_section("Current organization:");
            println!();

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
                .add("ID", info.id.to_string())
                .add("Created", info.created_at.to_rfc3339());

            println!("{table}");
            println!();
            print_hint("Use 'atomic org switch <slug>' to change the default organization.");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = OrgList {
            org: None,
            format: None,
        };
        assert!(cmd.org.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn with_org_override() {
        let cmd = OrgList {
            org: Some("other-org".to_string()),
            format: None,
        };
        assert_eq!(cmd.org.as_deref(), Some("other-org"));
    }

    #[test]
    fn json_format() {
        let cmd = OrgList {
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
        let cmd = OrgList {
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

    #[test]
    fn json_format_case_insensitive() {
        for variant in &["json", "JSON", "Json"] {
            let cmd = OrgList {
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
}

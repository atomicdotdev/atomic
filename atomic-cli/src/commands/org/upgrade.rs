//! The `org upgrade` command for upgrading an organization's plan.
//!
//! This module implements the `atomic org upgrade` command, which upgrades
//! a personal organization to a team organization on the remote
//! atomic-storage server, enabling multi-member collaboration features.
//!
//! # Usage
//!
//! ```text
//! atomic org upgrade [OPTIONS] [SLUG]
//!
//! Arguments:
//!   [SLUG]  Organization slug to upgrade (defaults to current org)
//!
//! Options:
//!   --org <ORG>    Organization slug override for the API client
//!   -h, --help     Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Upgrade the current default org
//! $ atomic org upgrade
//! ✓ Upgraded organization: alice
//!
//!   Name:  alice
//!   Slug:  alice
//!   Plan:  team
//!
//! Next steps:
//!   atomic org member add <id>   Add members to the org
//!   atomic team create <name>    Create a team
//!
//! # Upgrade a specific org
//! $ atomic org upgrade my-personal-org
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_next_steps, print_success, KeyValueTable};

/// Upgrade a personal org to a team org.
///
/// Converts a personal organization into a team organization on the
/// remote server, enabling multi-member collaboration features such
/// as team management and role-based access control.
///
/// The exact plan transition is determined server-side based on the
/// organization's current plan and eligibility. If the organization
/// is already a team org, the server may return an error.
#[derive(Debug, Parser)]
#[command(name = "upgrade")]
pub struct OrgUpgrade {
    /// Organization slug to upgrade.
    ///
    /// If omitted, the current default organization from global
    /// configuration is used.
    #[arg(value_name = "SLUG")]
    pub slug: Option<String>,

    /// Organization slug override for the API client.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client. This is distinct from the
    /// positional `slug` argument, which specifies *which* org to
    /// upgrade.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for OrgUpgrade {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgUpgrade {
    async fn execute(&self) -> CliResult<()> {
        let client = build_client(self.org.as_deref())?;

        // Determine which slug to upgrade: explicit arg > client's org slug.
        let slug = self.slug.as_deref().unwrap_or_else(|| client.org_slug());

        let info = atomic_teams::org::upgrade_org(&client, slug)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Upgraded organization: {}", info.slug));
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
        let table = table.add("ID", info.id.to_string());

        println!("{table}");

        print_next_steps(&[
            ("atomic org member add <id>", "Add members to the org"),
            ("atomic team create <name>", "Create a team"),
        ]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = OrgUpgrade {
            slug: None,
            org: None,
        };
        assert!(cmd.slug.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn explicit_slug() {
        let cmd = OrgUpgrade {
            slug: Some("my-personal-org".to_string()),
            org: None,
        };
        assert_eq!(cmd.slug.as_deref(), Some("my-personal-org"));
    }

    #[test]
    fn with_org_override() {
        let cmd = OrgUpgrade {
            slug: Some("target-org".to_string()),
            org: Some("client-org".to_string()),
        };
        assert_eq!(cmd.slug.as_deref(), Some("target-org"));
        assert_eq!(cmd.org.as_deref(), Some("client-org"));
    }

    #[test]
    fn slug_defaults_to_none() {
        let cmd = OrgUpgrade {
            slug: None,
            org: Some("other".to_string()),
        };
        assert!(cmd.slug.is_none());
        assert_eq!(cmd.org.as_deref(), Some("other"));
    }
}

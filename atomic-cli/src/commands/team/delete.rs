//! The `team delete` command for deleting a team.
//!
//! This module implements the `atomic team delete` command, which permanently
//! removes a team from the organization on the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic team delete [OPTIONS] <SLUG>
//!
//! Arguments:
//!   <SLUG>  Slug of the team to delete
//!
//! Options:
//!   --force        Skip confirmation prompt
//!   --org <ORG>    Organization slug override for the API client
//!   -h, --help     Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Delete with confirmation prompt
//! $ atomic team delete old-team
//! ⚠ This will permanently delete team 'old-team' and all its memberships and grants.
//! Are you sure? [y/N]: y
//! ✓ Deleted team: old-team
//!
//! # Force delete without confirmation
//! $ atomic team delete old-team --force
//! ✓ Deleted team: old-team
//!
//! # Delete a team in a specific org
//! $ atomic team delete old-team --org acme-corp --force
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

/// Delete a team.
///
/// Permanently removes a team and all associated memberships and
/// team-scoped grants from the organization. This action cannot be undone.
///
/// Requires admin or owner permissions on the organization. A confirmation
/// prompt is shown unless `--force` is passed.
#[derive(Debug, Parser)]
#[command(name = "delete")]
pub struct TeamDelete {
    /// Slug of the team to delete.
    ///
    /// The URL-safe slug of the team (e.g. `"backend-engineering"`).
    /// The team must exist and you must have sufficient permissions.
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,

    /// Skip the confirmation prompt.
    ///
    /// By default, the command asks for confirmation before deleting.
    /// Use this flag in scripts or when you are certain.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Organization slug override for the API client.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for TeamDelete {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl TeamDelete {
    async fn execute(&self) -> CliResult<()> {
        // Prompt for confirmation unless --force is set.
        if !self.force {
            print_warning(&format!(
                "This will permanently delete team '{}' and all its memberships and grants.",
                self.slug
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

        let client = build_client(self.org.as_deref())?;
        let org_slug = client.org_slug().to_string();

        atomic_teams::team::delete_team(&client, &org_slug, &self.slug)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Deleted team: {}", self.slug));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = TeamDelete {
            slug: "old-team".to_string(),
            force: false,
            org: None,
        };
        assert_eq!(cmd.slug, "old-team");
        assert!(!cmd.force);
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_force() {
        let cmd = TeamDelete {
            slug: "old-team".to_string(),
            force: true,
            org: None,
        };
        assert!(cmd.force);
    }

    #[test]
    fn with_org_override() {
        let cmd = TeamDelete {
            slug: "target-team".to_string(),
            force: false,
            org: Some("acme-corp".to_string()),
        };
        assert_eq!(cmd.slug, "target-team");
        assert_eq!(cmd.org.as_deref(), Some("acme-corp"));
    }

    #[test]
    fn force_and_org_combined() {
        let cmd = TeamDelete {
            slug: "old-team".to_string(),
            force: true,
            org: Some("other".to_string()),
        };
        assert!(cmd.force);
        assert_eq!(cmd.org.as_deref(), Some("other"));
    }

    #[test]
    fn slug_with_hyphens() {
        let cmd = TeamDelete {
            slug: "my-old-team-123".to_string(),
            force: false,
            org: None,
        };
        assert_eq!(cmd.slug, "my-old-team-123");
    }
}

//! The `org delete` command for deleting an organization.
//!
//! This module implements the `atomic org delete` command, which permanently
//! removes an organization from the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic org delete [OPTIONS] <SLUG>
//!
//! Arguments:
//!   <SLUG>  Slug of the organization to delete
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
//! $ atomic org delete acme-corp
//! ⚠ This will permanently delete organization 'acme-corp' and all its data.
//! Are you sure? [y/N]: y
//! ✓ Deleted organization: acme-corp
//!
//! # Force delete without confirmation
//! $ atomic org delete acme-corp --force
//! ✓ Deleted organization: acme-corp
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

/// Delete an organization.
///
/// Permanently removes an organization and all associated data from the
/// remote server. This action cannot be undone.
///
/// Requires owner permissions on the organization. A confirmation prompt
/// is shown unless `--force` is passed.
#[derive(Debug, Parser)]
#[command(name = "delete")]
pub struct OrgDelete {
    /// Slug of the organization to delete.
    ///
    /// The organization must exist and you must be an owner.
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

impl Command for OrgDelete {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgDelete {
    async fn execute(&self) -> CliResult<()> {
        // Prompt for confirmation unless --force is set.
        if !self.force {
            print_warning(&format!(
                "This will permanently delete organization '{}' and all its data.",
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

        atomic_teams::org::delete_org(&client, &self.slug)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Deleted organization: {}", self.slug));

        // Clean up local config: if the deleted org was the current default
        // or had a configured default workspace, those entries are now stale
        // and would surface as confusing errors on subsequent commands.
        // We log but do not fail on config errors here — the server-side
        // deletion has already succeeded.
        if let Err(e) = clean_up_local_config(&self.slug) {
            log::warn!("Failed to clean up local config after org delete: {e}");
        }

        Ok(())
    }
}

/// Remove references to a deleted org from the local global config.
///
/// Returns the side effects performed so the caller can print appropriate
/// hints. Specifically:
///   - If `server.default_org == Some(slug)`, set it to `None` and hint.
///   - Remove `server.default_workspaces[slug]` if present.
fn clean_up_local_config(deleted_slug: &str) -> CliResult<()> {
    let mut config = GlobalConfig::load().map_err(|e| {
        CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
    })?;

    let mut changed = false;
    let was_default_org = config.server.default_org.as_deref() == Some(deleted_slug);
    if was_default_org {
        config.server.default_org = None;
        changed = true;
    }
    if config
        .server
        .default_workspaces
        .remove(deleted_slug)
        .is_some()
    {
        changed = true;
    }

    if !changed {
        return Ok(());
    }

    config.save().map_err(|e| {
        CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
    })?;

    if was_default_org {
        print_hint(&format!(
            "Default org was '{deleted_slug}' — run 'atomic org switch <slug>' \
             to pick a new one."
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = OrgDelete {
            slug: "acme-corp".to_string(),
            force: false,
            org: None,
        };
        assert_eq!(cmd.slug, "acme-corp");
        assert!(!cmd.force);
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_force() {
        let cmd = OrgDelete {
            slug: "acme-corp".to_string(),
            force: true,
            org: None,
        };
        assert!(cmd.force);
    }

    #[test]
    fn with_org_override() {
        let cmd = OrgDelete {
            slug: "target-org".to_string(),
            force: false,
            org: Some("client-org".to_string()),
        };
        assert_eq!(cmd.slug, "target-org");
        assert_eq!(cmd.org.as_deref(), Some("client-org"));
    }

    #[test]
    fn force_and_org_combined() {
        let cmd = OrgDelete {
            slug: "acme".to_string(),
            force: true,
            org: Some("other".to_string()),
        };
        assert!(cmd.force);
        assert_eq!(cmd.org.as_deref(), Some("other"));
    }
}

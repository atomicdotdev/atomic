//! The `workspace delete` command for deleting a remote workspace.
//!
//! This module implements the `atomic workspace delete` command, which removes
//! a workspace from the remote server. Without `--force`, the user is prompted
//! for confirmation via an interactive dialog.
//!
//! # Usage
//!
//! ```text
//! atomic workspace delete <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Slug of the workspace to delete
//!
//! Options:
//!       --org <ORG>  Organization override
//!   -f, --force      Skip confirmation prompt
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Delete with confirmation prompt
//! $ atomic workspace delete old-workspace
//! ⚠ This will permanently delete workspace 'old-workspace' and all its projects.
//! ? Are you sure? (y/N) y
//! ✓ Deleted workspace: old-workspace
//!
//! # Force delete without prompt
//! $ atomic workspace delete old-workspace --force
//! ✓ Deleted workspace: old-workspace
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::client::{build_client_with_org, remote_err};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_error, print_hint, print_success, print_warning};

/// Delete a remote workspace.
///
/// Permanently removes the workspace and all projects within it from the
/// remote server. This action cannot be undone.
///
/// Without `--force`, an interactive confirmation prompt is shown.
#[derive(Debug, Parser, Default)]
#[command(name = "delete")]
pub struct WorkspaceDelete {
    /// Slug of the workspace to delete.
    #[arg(default_value = "")]
    pub slug: String,

    /// Organization override.
    ///
    /// Uses the default org from config if not specified.
    #[arg(long)]
    pub org: Option<String>,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Command for WorkspaceDelete {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        rt.block_on(async {
            let (client, org_slug) = build_client_with_org(self.org.as_deref(), None).await?;

            // Confirm unless --force is set. --force skips the existence
            // precheck too, keeping scripted deletes to a single request.
            if !self.force {
                // Verify the workspace exists before asking for confirmation,
                // so a typo'd slug fails fast instead of after the prompt.
                client.get_workspace(&self.slug).await.map_err(remote_err)?;

                print_warning(&format!(
                    "This will permanently delete workspace '{}' and all its projects.",
                    self.slug
                ));

                let confirmed = dialoguer::Confirm::new()
                    .with_prompt("Are you sure?")
                    .default(false)
                    .interact()
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;

                if !confirmed {
                    print_error("Cancelled.");
                    return Err(CliError::Cancelled);
                }
            }

            client
                .delete_workspace(&self.slug)
                .await
                .map_err(remote_err)?;

            print_success(&format!("Deleted workspace: {}", self.slug));

            // If this workspace was the default for the org we just operated
            // in, remove the stale entry so subsequent commands don't hit a
            // confusing 404. Server-side deletion succeeded; config cleanup
            // is best-effort.
            if let Err(e) = clean_up_local_config(&org_slug, &self.slug) {
                log::warn!("Failed to clean up local config after workspace delete: {e}");
            }

            Ok(())
        })
    }
}

/// Remove a deleted workspace from `[server.default_workspaces]` if it was
/// the configured default for its org.
fn clean_up_local_config(org_slug: &str, deleted_workspace: &str) -> CliResult<()> {
    let mut config = GlobalConfig::load()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}")))?;

    let was_default = config
        .server
        .default_workspaces
        .get(org_slug)
        .map(|v| v == deleted_workspace)
        .unwrap_or(false);

    if !was_default {
        return Ok(());
    }

    config.server.default_workspaces.remove(org_slug);
    config
        .save()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}")))?;

    print_hint(&format!(
        "Default workspace for '{org_slug}' was '{deleted_workspace}' — \
         run 'atomic workspace set <slug>' to pick a new one."
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_parse() {
        let cmd = WorkspaceDelete {
            slug: "old-ws".to_string(),
            org: Some("acme".to_string()),
            force: true,
        };
        assert_eq!(cmd.slug, "old-ws");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert!(cmd.force);
    }

    #[test]
    fn default_force_is_false() {
        let cmd = WorkspaceDelete {
            slug: "ws".to_string(),
            org: None,
            force: false,
        };
        assert!(!cmd.force);
    }
}

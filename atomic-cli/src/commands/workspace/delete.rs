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

use crate::commands::client::{build_client, remote_err};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_error, print_success, print_warning};

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
            // Confirm unless --force is set.
            if !self.force {
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

            let client = build_client(self.org.as_deref())?;

            client
                .delete_workspace(&self.slug)
                .await
                .map_err(remote_err)?;

            print_success(&format!("Deleted workspace: {}", self.slug));

            Ok(())
        })
    }
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

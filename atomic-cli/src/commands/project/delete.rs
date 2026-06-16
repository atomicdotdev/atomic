//! The `project delete` command for deleting a remote project.
//!
//! # Usage
//!
//! ```text
//! atomic project delete <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Project path in 'workspace/project' format
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
//! # Delete a project (with confirmation prompt)
//! $ atomic project delete my-workspace/old-project
//! ? Delete project 'old-project' from workspace 'my-workspace'? This cannot be undone. yes
//! ✓ Deleted project old-project from workspace my-workspace
//!
//! # Force delete without confirmation
//! $ atomic project delete my-workspace/old-project --force
//! ✓ Deleted project old-project from workspace my-workspace
//! ```

use clap::Parser;

use crate::commands::client::{build_client, remote_err};
use crate::commands::project::parse_project_path;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_error, print_success};

/// Delete a remote project.
///
/// Permanently removes a project and all its data from the server.
/// Requires the project path in `workspace/project` format.
///
/// Without `--force`, prompts for confirmation before deleting.
#[derive(Debug, Parser)]
#[command(name = "delete")]
pub struct ProjectDelete {
    /// Project path in 'workspace/project' format.
    ///
    /// Example: `my-workspace/my-project`
    #[arg(required = true)]
    pub slug: String,

    /// Organization to delete the project from.
    ///
    /// Overrides the default organization from global config.
    #[arg(long)]
    pub org: Option<String>,

    /// Force deletion without confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,
}

impl Command for ProjectDelete {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let (ws_slug, proj_slug) = parse_project_path(&self.slug)?;

            // Confirm deletion unless --force is specified.
            if !self.force {
                let prompt = format!(
                    "Delete project '{}' from workspace '{}'? This cannot be undone.",
                    proj_slug, ws_slug,
                );

                let confirmed = dialoguer::Confirm::new()
                    .with_prompt(&prompt)
                    .default(false)
                    .interact()
                    .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;

                if !confirmed {
                    return Err(CliError::Cancelled);
                }
            }

            let client = build_client(self.org.as_deref()).await?;

            client
                .delete_project(ws_slug, proj_slug)
                .await
                .map_err(|e| {
                    if e.is_not_found() {
                        print_error(&format!(
                            "Project '{}' not found in workspace '{}'",
                            proj_slug, ws_slug,
                        ));
                    }
                    remote_err(e)
                })?;

            print_success(&format!(
                "Deleted project {} from workspace {}",
                proj_slug, ws_slug,
            ));

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_default() {
        let cmd = ProjectDelete {
            slug: "ws/proj".to_string(),
            org: None,
            force: false,
        };
        assert_eq!(cmd.slug, "ws/proj");
        assert!(!cmd.force);
        assert!(cmd.org.is_none());
    }

    #[test]
    fn fields_with_force_and_org() {
        let cmd = ProjectDelete {
            slug: "team/backend".to_string(),
            org: Some("acme".to_string()),
            force: true,
        };
        assert_eq!(cmd.slug, "team/backend");
        assert!(cmd.force);
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }
}

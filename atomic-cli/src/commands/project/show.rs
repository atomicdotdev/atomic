//! The `project show` command for displaying project details.
//!
//! Shows detailed information about a remote project using a
//! `workspace/project` slug format.
//!
//! # Usage
//!
//! ```text
//! atomic project show <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Project path in 'workspace/project' format
//!
//! Options:
//!   --org <ORG>        Organization override
//!   --format <FORMAT>  Output format: table or json (default: table)
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! $ atomic project show my-workspace/my-project
//! Name:        my-project
//! Slug:        my-project
//! Workspace:   00000000-0000-0000-0000-000000000001
//! View:        dev
//! Visibility:  private
//! VCS URL:     https://alice.atomic.storage/workspaces/my-workspace/projects/my-project/code
//! Created:     2025-01-15 10:30:45
//! Updated:     2025-01-15 10:30:45
//!
//! $ atomic project show my-workspace/my-project --format json
//! { ... }
//! ```

use clap::Parser;

use crate::commands::client::{build_client, remote_err};
use crate::commands::format_timestamp;
use crate::commands::project::parse_project_path;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::KeyValueTable;

/// Show details of a remote project.
///
/// Displays detailed information about a project using its full path
/// in `workspace/project` format.
#[derive(Debug, Parser)]
#[command(name = "show")]
pub struct ProjectShow {
    /// Project path in `workspace/project` format.
    ///
    /// The workspace slug and project slug separated by a forward slash.
    ///
    /// Examples:
    ///   my-workspace/my-project
    ///   team-ws/backend-api
    #[arg(required = true)]
    pub slug: String,

    /// Organization to query.
    ///
    /// Overrides the default organization from global config.
    #[arg(long)]
    pub org: Option<String>,

    /// Output format: `table` or `json`.
    #[arg(long, default_value = "table")]
    pub format: String,
}

impl Command for ProjectShow {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let (ws_slug, proj_slug) = parse_project_path(&self.slug)?;
            let client = build_client(self.org.as_deref(), None).await?;

            let project = client
                .get_project(ws_slug, proj_slug)
                .await
                .map_err(remote_err)?;

            if self.format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&project).unwrap_or_default()
                );
                return Ok(());
            }

            // Build VCS URL
            let vcs_url = format!(
                "{}/workspaces/{}/projects/{}/code",
                client.base_url(),
                ws_slug,
                proj_slug,
            );

            let table = KeyValueTable::new()
                .add("Name", &project.name)
                .add("Slug", &project.slug)
                .add("Workspace", project.workspace_id.to_string())
                .add("View", &project.default_view)
                .add("Visibility", project.visibility.to_string())
                .add("Description", project.description.as_deref().unwrap_or("—"))
                .add("VCS URL", &vcs_url)
                .add("Created", format_timestamp(&project.created_at))
                .add("Updated", format_timestamp(&project.updated_at));

            println!("{}", table);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fields() {
        let cmd = ProjectShow {
            slug: "ws/proj".to_string(),
            org: None,
            format: "table".to_string(),
        };
        assert_eq!(cmd.slug, "ws/proj");
        assert!(cmd.org.is_none());
        assert_eq!(cmd.format, "table");
    }

    #[test]
    fn test_json_format() {
        let cmd = ProjectShow {
            slug: "ws/proj".to_string(),
            org: Some("acme".to_string()),
            format: "json".to_string(),
        };
        assert_eq!(cmd.format, "json");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn test_invalid_slug_format() {
        // parse_project_path should fail for a slug without '/'
        let result = parse_project_path("no-slash");
        assert!(result.is_err());
    }

    #[test]
    fn test_valid_slug_parsing() {
        let (ws, proj) = parse_project_path("my-ws/my-proj").unwrap();
        assert_eq!(ws, "my-ws");
        assert_eq!(proj, "my-proj");
    }
}

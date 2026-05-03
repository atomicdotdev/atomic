//! The `project list` command for listing projects in a workspace.
//!
//! This module implements the `atomic project list` command, which lists
//! all projects within a remote workspace.
//!
//! # Usage
//!
//! ```text
//! atomic project list --workspace <SLUG> [OPTIONS]
//!
//! Options:
//!   -w, --workspace <SLUG>  Workspace slug (required)
//!       --org <ORG>         Organization override
//!       --format <FORMAT>   Output format: table or json (default: table)
//!   -h, --help              Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! $ atomic project list --workspace my-ws
//! NAME          SLUG          VIEW   VISIBILITY  CREATED
//! ─────────────────────────────────────────────────────────
//! my-project    my-project    dev    private     2025-01-15 10:30:00
//!
//! $ atomic project list --workspace my-ws --format json
//! [{"name":"my-project", ...}]
//! ```

use clap::Parser;

use crate::commands::client::{build_client, remote_err};
use crate::commands::{format_timestamp, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, Column, Table};

/// List all projects in a remote workspace.
///
/// Shows projects with their name, slug, default view, visibility, and
/// creation date. Requires the workspace slug to scope the listing.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct ProjectList {
    /// Workspace slug to list projects from.
    #[arg(short, long, required = true)]
    pub workspace: String,

    /// Organization override (defaults to configured org).
    #[arg(long)]
    pub org: Option<String>,

    /// Output format: `table` or `json`.
    #[arg(long, default_value = "table")]
    pub format: String,
}

impl Command for ProjectList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref())?;
            let projects = client
                .list_projects(&self.workspace)
                .await
                .map_err(remote_err)?;

            if self.format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&projects).unwrap_or_default()
                );
                return Ok(());
            }

            if projects.is_empty() {
                print_hint(&format!(
                    "No projects found in workspace '{}'. Create one with \
                     'atomic project create <name> --workspace {}'.",
                    self.workspace, self.workspace
                ));
                return Ok(());
            }

            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("NAME"),
                Column::new("SLUG"),
                Column::new("VIEW"),
                Column::new("VISIBILITY"),
                Column::new("CREATED"),
            ]);

            for proj in &projects {
                table.add_row(vec![
                    proj.name.clone(),
                    proj.slug.clone(),
                    proj.default_view.clone(),
                    proj.visibility.to_string(),
                    format_timestamp(&proj.created_at),
                ]);
            }

            println!("{}", table);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_format_is_table() {
        let cmd = ProjectList {
            workspace: "ws".to_string(),
            org: None,
            format: "table".to_string(),
        };
        assert_eq!(cmd.format, "table");
    }

    #[test]
    fn workspace_is_stored() {
        let cmd = ProjectList {
            workspace: "my-workspace".to_string(),
            org: Some("acme".to_string()),
            format: "json".to_string(),
        };
        assert_eq!(cmd.workspace, "my-workspace");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert_eq!(cmd.format, "json");
    }
}

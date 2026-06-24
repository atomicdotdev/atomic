//! The `project list` command for listing projects in a workspace.
//!
//! This module implements the `atomic project list` command, which lists
//! all projects within a remote workspace.
//!
//! # Usage
//!
//! ```text
//! atomic project list [--workspace <SLUG>] [OPTIONS]
//!
//! Options:
//!   -w, --workspace <SLUG>  Workspace slug. Falls back to the default
//!                           workspace for the current org if omitted.
//!       --org <ORG>         Organization override
//!       --format <FORMAT>   Output format: table or json (default: table)
//!   -h, --help              Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Explicit workspace
//! $ atomic project list --workspace my-ws
//! NAME          SLUG          VIEW   VISIBILITY  CREATED
//! ─────────────────────────────────────────────────────────
//! my-project    my-project    dev    private     2025-01-15 10:30:00
//!
//! # Uses the default workspace for the current org
//! $ atomic workspace set my-ws
//! $ atomic project list
//! ```

use clap::Parser;

use crate::commands::client::{build_client_with_org, remote_err, resolve_workspace};
use crate::commands::{format_timestamp, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, Column, Table};

/// List all projects in a remote workspace.
///
/// Shows projects with their name, slug, default view, visibility, and
/// creation date. If `--workspace` is omitted, falls back to the default
/// workspace for the current org.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct ProjectList {
    /// Workspace slug to list projects from.
    ///
    /// Falls back to the default workspace for the current org if omitted.
    #[arg(short, long)]
    pub workspace: Option<String>,

    /// Organization override (defaults to configured org).
    #[arg(long)]
    pub org: Option<String>,

    /// Server profile to use (e.g. "staging", "prod").
    ///
    /// Overrides `default_server` from `~/.atomic/config.toml`.
    /// Use `atomic server list` to see available profiles.
    #[arg(long)]
    pub server: Option<String>,

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
            let (client, org_slug) =
                build_client_with_org(self.org.as_deref(), self.server.as_deref()).await?;
            let workspace = resolve_workspace(&org_slug, self.workspace.as_deref())?;

            let projects = client.list_projects(&workspace).await.map_err(remote_err)?;

            if self.format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&projects).unwrap_or_default()
                );
                return Ok(());
            }

            if projects.is_empty() {
                // Mirror the flags the user typed so the suggested follow-up
                // resolves to the same org/workspace they were just looking
                // at — not whatever the current defaults are.
                let mut create_hint = String::from("atomic project create <name>");
                if let Some(ws) = self.workspace.as_deref() {
                    create_hint.push_str(&format!(" --workspace {ws}"));
                }
                if let Some(org) = self.org.as_deref() {
                    create_hint.push_str(&format!(" --org {org}"));
                }
                print_hint(&format!(
                    "No projects found in workspace '{workspace}'. \
                     Create one with '{create_hint}'."
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
            workspace: Some("ws".to_string()),
            org: None,
            server: None,
            format: "table".to_string(),
        };
        assert_eq!(cmd.format, "table");
    }

    #[test]
    fn workspace_is_stored() {
        let cmd = ProjectList {
            workspace: Some("my-workspace".to_string()),
            org: Some("acme".to_string()),
            server: None,
            format: "json".to_string(),
        };
        assert_eq!(cmd.workspace.as_deref(), Some("my-workspace"));
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert_eq!(cmd.format, "json");
    }

    #[test]
    fn workspace_can_be_omitted() {
        let cmd = ProjectList {
            workspace: None,
            org: None,
            server: None,
            format: "table".to_string(),
        };
        assert!(cmd.workspace.is_none());
    }
}

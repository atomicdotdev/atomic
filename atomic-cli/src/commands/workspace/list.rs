//! The `workspace list` command for listing remote workspaces.
//!
//! # Usage
//!
//! ```text
//! atomic workspace list [OPTIONS]
//!
//! Options:
//!   --org <ORG>        Organization to list workspaces from
//!   --format <FORMAT>  Output format: table or json (default: table)
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all workspaces
//! $ atomic workspace list
//! NAME          SLUG          VISIBILITY  CREATED
//! ─────────────────────────────────────────────────
//! My Workspace  my-workspace  private     2025-01-15 10:30:45
//!
//! # List as JSON
//! $ atomic workspace list --format json
//! ```

use clap::Parser;

use crate::commands::client::{build_client, remote_err};
use crate::commands::{format_timestamp, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, Column, Table};

/// List remote workspaces.
///
/// Shows all workspaces visible to the authenticated identity in the
/// configured (or overridden) organization.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct WorkspaceList {
    /// Organization to list workspaces from.
    ///
    /// Overrides the default org set during registration.
    #[arg(long)]
    pub org: Option<String>,

    /// Output format (`table` or `json`).
    #[arg(long, default_value = "table")]
    pub format: String,
}

impl Default for WorkspaceList {
    fn default() -> Self {
        Self {
            org: None,
            format: "table".to_string(),
        }
    }
}

impl WorkspaceList {
    /// Builder: set the org override.
    pub fn with_org(mut self, org: impl Into<String>) -> Self {
        self.org = Some(org.into());
        self
    }

    /// Builder: set the output format.
    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.format = format.into();
        self
    }
}

impl Command for WorkspaceList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref())?;
            let workspaces = client.list_workspaces().await.map_err(remote_err)?;

            if self.format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&workspaces).unwrap_or_default()
                );
                return Ok(());
            }

            if workspaces.is_empty() {
                print_hint(
                    "No workspaces found. Create one with 'atomic workspace create <name>'.",
                );
                return Ok(());
            }

            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("NAME"),
                Column::new("SLUG"),
                Column::new("VISIBILITY"),
                Column::new("CREATED"),
            ]);

            for ws in &workspaces {
                table.add_row(vec![
                    ws.name.clone(),
                    ws.slug.clone(),
                    ws.visibility.to_string(),
                    format_timestamp(&ws.created_at),
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
    fn test_default() {
        let cmd = WorkspaceList::default();
        assert!(cmd.org.is_none());
        assert_eq!(cmd.format, "table");
    }

    #[test]
    fn test_with_org() {
        let cmd = WorkspaceList::default().with_org("acme");
        assert_eq!(cmd.org, Some("acme".to_string()));
    }

    #[test]
    fn test_with_format() {
        let cmd = WorkspaceList::default().with_format("json");
        assert_eq!(cmd.format, "json");
    }
}

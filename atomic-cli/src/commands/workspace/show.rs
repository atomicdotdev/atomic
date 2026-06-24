//! The `workspace show` command for displaying workspace details.
//!
//! Fetches a single workspace by slug from the remote server and displays
//! its details in either a key-value table or JSON format.
//!
//! # Usage
//!
//! ```text
//! atomic workspace show <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Workspace slug to look up
//!
//! Options:
//!   --org <ORG>        Organization override
//!   --format <FORMAT>  Output format: table or json [default: table]
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Show workspace details
//! $ atomic workspace show my-workspace
//! Name:        My Workspace
//! Slug:        my-workspace
//! Visibility:  private
//! Description: A workspace for my projects
//! Created:     2025-06-01 12:00:00
//! Updated:     2025-06-15 09:30:00
//!
//! # Show as JSON
//! $ atomic workspace show my-workspace --format json
//! ```

use clap::Parser;

use crate::commands::client::{build_client, remote_err};
use crate::commands::{format_timestamp, Command};
use crate::error::{CliError, CliResult};
use crate::output::KeyValueTable;

/// Show details for a remote workspace.
///
/// Fetches the workspace identified by `slug` and displays its metadata
/// in a human-readable key-value table or as raw JSON.
#[derive(Debug, Parser, Default)]
#[command(name = "show")]
pub struct WorkspaceShow {
    /// Slug of the workspace to show.
    #[arg(default_value = "")]
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

impl Command for WorkspaceShow {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref()).await?;
            let ws = client.get_workspace(&self.slug).await.map_err(remote_err)?;

            if self.format == "json" {
                println!("{}", serde_json::to_string_pretty(&ws).unwrap_or_default());
                return Ok(());
            }

            let description = ws.description.as_deref().unwrap_or("—").to_string();

            let table = KeyValueTable::new()
                .add("Name", &ws.name)
                .add("Slug", &ws.slug)
                .add("Visibility", ws.visibility.to_string())
                .add("Description", &description)
                .add("Owner ID", ws.owner_id.to_string())
                .add("Created", format_timestamp(&ws.created_at))
                .add("Updated", format_timestamp(&ws.updated_at));

            println!("{}", table);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_parse() {
        let cmd = WorkspaceShow {
            slug: "my-ws".to_string(),
            org: Some("acme".to_string()),
            format: "json".to_string(),
        };
        assert_eq!(cmd.slug, "my-ws");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert_eq!(cmd.format, "json");
    }

    #[test]
    fn default_format_is_table() {
        let cmd = WorkspaceShow {
            slug: "ws".to_string(),
            org: None,
            format: "table".to_string(),
        };
        assert_eq!(cmd.format, "table");
    }
}

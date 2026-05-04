//! The `workspace update` command for modifying a remote workspace.
//!
//! # Usage
//!
//! ```text
//! atomic workspace update <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Slug of the workspace to update
//!
//! Options:
//!   --name <NAME>              New display name
//!   --description <DESC>       New description
//!   --visibility <VISIBILITY>  New visibility (public or private)
//!   --org <ORG>                Organization override
//!   -h, --help                 Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Rename a workspace
//! $ atomic workspace update my-ws --name "My Renamed Workspace"
//! ✓ Updated workspace: my-ws
//!
//! # Change visibility
//! $ atomic workspace update my-ws --visibility public
//! ✓ Updated workspace: my-ws
//! ```

use clap::Parser;

use atomic_remote::storage_types::{UpdateWorkspaceRequest, Visibility};

use crate::commands::client::{build_client, remote_err};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, KeyValueTable};

/// Update a remote workspace.
///
/// At least one of `--name`, `--description`, or `--visibility` must be
/// provided. Only the specified fields are changed; the rest remain as-is.
#[derive(Debug, Parser, Default)]
#[command(name = "update")]
pub struct WorkspaceUpdate {
    /// Slug of the workspace to update.
    pub slug: String,

    /// New display name for the workspace.
    #[arg(long)]
    pub name: Option<String>,

    /// New description for the workspace.
    #[arg(long)]
    pub description: Option<String>,

    /// New visibility (public or private).
    #[arg(long)]
    pub visibility: Option<Visibility>,

    /// Organization override (defaults to configured org).
    #[arg(long)]
    pub org: Option<String>,
}

impl Command for WorkspaceUpdate {
    fn run(&self) -> CliResult<()> {
        // Validate that at least one field is being updated.
        if self.name.is_none() && self.description.is_none() && self.visibility.is_none() {
            return Err(CliError::InvalidArgument {
                message: "At least one of --name, --description, or --visibility is required."
                    .to_string(),
            });
        }

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref())?;

            let req = UpdateWorkspaceRequest {
                name: self.name.clone(),
                description: self.description.clone(),
                visibility: self.visibility,
            };

            let ws = client
                .update_workspace(&self.slug, &req)
                .await
                .map_err(remote_err)?;

            print_success(&format!("Updated workspace: {}", ws.slug));
            println!();

            let table = KeyValueTable::new()
                .add("Name", &ws.name)
                .add("Slug", &ws.slug)
                .add("Visibility", ws.visibility.to_string())
                .add("Description", ws.description.as_deref().unwrap_or("—"));

            println!("{}", table);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_no_update_fields() {
        let cmd = WorkspaceUpdate {
            slug: "my-ws".to_string(),
            name: None,
            description: None,
            visibility: None,
            org: None,
        };
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("At least one"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn struct_fields() {
        let cmd = WorkspaceUpdate {
            slug: "ws".to_string(),
            name: Some("New Name".to_string()),
            description: Some("New desc".to_string()),
            visibility: Some(Visibility::Public),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.slug, "ws");
        assert_eq!(cmd.name.as_deref(), Some("New Name"));
        assert_eq!(cmd.description.as_deref(), Some("New desc"));
        assert_eq!(cmd.visibility, Some(Visibility::Public));
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }
}

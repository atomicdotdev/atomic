//! The `workspace create` command for creating a remote workspace.
//!
//! # Usage
//!
//! ```text
//! atomic workspace create <NAME> [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name for the new workspace
//!
//! Options:
//!   --description <TEXT>       Optional description
//!   --visibility <VISIBILITY>  public or private (default: private)
//!   --org <SLUG>               Organization override
//!   -h, --help                 Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Create a private workspace
//! $ atomic workspace create my-team
//! ✓ Created workspace: my-team (slug: my-team)
//!
//! # Create a public workspace with description
//! $ atomic workspace create oss-libs --visibility public --description "Open-source libraries"
//! ✓ Created workspace: oss-libs (slug: oss-libs)
//! ```

use clap::Parser;

use atomic_remote::storage_types::{CreateWorkspaceRequest, Visibility};

use crate::commands::client::{build_client, remote_err};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_next_steps, print_success, KeyValueTable};

/// Create a new remote workspace.
///
/// A workspace is a container for projects within an organization.
/// By default, workspaces are private.
#[derive(Debug, Parser, Default)]
#[command(name = "create")]
pub struct WorkspaceCreate {
    /// Name for the new workspace.
    #[arg(default_value = "")]
    pub name: String,

    /// Optional description for the workspace.
    #[arg(long)]
    pub description: Option<String>,

    /// Visibility: `public` or `private` (default: private).
    #[arg(long, default_value = "private")]
    pub visibility: String,

    /// Organization slug (overrides the configured default).
    #[arg(long)]
    pub org: Option<String>,
}

impl WorkspaceCreate {
    fn parse_visibility(&self) -> CliResult<Visibility> {
        self.visibility
            .parse::<Visibility>()
            .map_err(|e| CliError::InvalidArgument { message: e })
    }
}

impl Command for WorkspaceCreate {
    fn run(&self) -> CliResult<()> {
        if self.name.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Workspace name is required.".to_string(),
            });
        }

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref(), None).await?;

            let visibility = self.parse_visibility()?;

            let req = CreateWorkspaceRequest {
                name: self.name.clone(),
                description: self.description.clone(),
                visibility,
            };

            let ws = client.create_workspace(&req).await.map_err(remote_err)?;

            print_success(&format!(
                "Created workspace: {} (slug: {})",
                ws.name, ws.slug
            ));
            println!();

            let table = KeyValueTable::new()
                .add("Name", &ws.name)
                .add("Slug", &ws.slug)
                .add("Visibility", ws.visibility.to_string())
                .add("Created", crate::commands::format_timestamp(&ws.created_at));

            println!("{}", table);

            print_next_steps(&[
                (
                    &format!("atomic workspace set {}", ws.slug),
                    "Set as your default workspace",
                ),
                (
                    "atomic project create <project-name>",
                    "Create a project in this workspace",
                ),
                ("atomic workspace list", "View all workspaces"),
            ]);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_empty_name() {
        let cmd = WorkspaceCreate::default();
        assert_eq!(cmd.name, "");
        assert!(cmd.description.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn default_visibility_is_private() {
        let cmd = WorkspaceCreate {
            name: "test".to_string(),
            description: None,
            visibility: "private".to_string(),
            org: None,
        };
        assert_eq!(cmd.visibility, "private");
    }

    #[test]
    fn fields_are_stored() {
        let cmd = WorkspaceCreate {
            name: "my-ws".to_string(),
            description: Some("A workspace".to_string()),
            visibility: "public".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.name, "my-ws");
        assert_eq!(cmd.description.as_deref(), Some("A workspace"));
        assert_eq!(cmd.visibility, "public");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn parse_visibility_valid() {
        let cmd = WorkspaceCreate {
            name: "test".to_string(),
            visibility: "public".to_string(),
            ..Default::default()
        };
        let v = cmd.parse_visibility().unwrap();
        assert_eq!(v, Visibility::Public);
    }

    #[test]
    fn parse_visibility_invalid() {
        let cmd = WorkspaceCreate {
            name: "test".to_string(),
            visibility: "bogus".to_string(),
            ..Default::default()
        };
        let err = cmd.parse_visibility();
        assert!(err.is_err());
    }

    #[test]
    fn empty_name_rejected() {
        let cmd = WorkspaceCreate::default();
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("required"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }
}

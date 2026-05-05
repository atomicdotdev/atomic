//! The `project update` command for modifying a remote project.
//!
//! # Usage
//!
//! ```text
//! atomic project update <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <SLUG>  Project path in 'workspace/project' format
//!
//! Options:
//!   --name <NAME>              New project name
//!   --description <DESC>       New description
//!   --default-view <VIEW>      New default view
//!   --visibility <VISIBILITY>  New visibility (public or private)
//!   --org <ORG>                Organization override
//!   -h, --help                 Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Rename a project
//! $ atomic project update my-ws/old-name --name new-name
//! ✓ Updated project: new-name
//!
//! # Change visibility and default view
//! $ atomic project update my-ws/my-proj --visibility public --default-view main
//! ✓ Updated project: my-proj
//! ```

use clap::Parser;

use atomic_remote::storage_types::{UpdateProjectRequest, Visibility};

use crate::commands::client::{build_client, remote_err};
use crate::commands::project::parse_project_path;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::print_success;

/// Update an existing remote project.
///
/// At least one of `--name`, `--description`, `--default-view`, or
/// `--visibility` must be provided. Only the specified fields are
/// changed; everything else is left as-is.
#[derive(Debug, Parser)]
#[command(name = "update")]
pub struct ProjectUpdate {
    /// Project path in 'workspace/project' format.
    ///
    /// For example: `my-workspace/my-project`.
    #[arg(required = true)]
    pub slug: String,

    /// New name for the project.
    #[arg(long)]
    pub name: Option<String>,

    /// New description for the project.
    #[arg(long)]
    pub description: Option<String>,

    /// New default view for the project.
    #[arg(long)]
    pub default_view: Option<String>,

    /// New visibility (public or private).
    #[arg(long)]
    pub visibility: Option<Visibility>,

    /// Organization override (defaults to configured org).
    #[arg(long)]
    pub org: Option<String>,
}

impl Command for ProjectUpdate {
    fn run(&self) -> CliResult<()> {
        // Ensure at least one field is being updated.
        if self.name.is_none()
            && self.description.is_none()
            && self.default_view.is_none()
            && self.visibility.is_none()
        {
            return Err(CliError::InvalidArgument {
                message: "At least one of --name, --description, --default-view, or --visibility must be provided.".to_string(),
            });
        }

        let (ws_slug, proj_slug) = parse_project_path(&self.slug)?;

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref())?;

            let req = UpdateProjectRequest {
                name: self.name.clone(),
                description: self.description.clone(),
                default_view: self.default_view.clone(),
                visibility: self.visibility,
            };

            let project = client
                .update_project(ws_slug, proj_slug, &req)
                .await
                .map_err(remote_err)?;

            print_success(&format!("Updated project: {}", project.name));

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_at_least_one_update_field() {
        let cmd = ProjectUpdate {
            slug: "ws/proj".to_string(),
            name: None,
            description: None,
            default_view: None,
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
    fn rejects_invalid_slug_format() {
        let cmd = ProjectUpdate {
            slug: "no-slash-here".to_string(),
            name: Some("new-name".to_string()),
            description: None,
            default_view: None,
            visibility: None,
            org: None,
        };
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("workspace/project"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }
}

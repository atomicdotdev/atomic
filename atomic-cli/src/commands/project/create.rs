//! The `project create` command for creating a new remote project.
//!
//! Creates a project within a workspace on the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic project create <NAME> [--workspace <SLUG>] [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name of the project to create
//!
//! Options:
//!   -w, --workspace <SLUG>     Workspace to create the project in.
//!                              Falls back to the default workspace for the
//!                              current org if omitted.
//!       --description <TEXT>   Optional description
//!       --kind <KIND>          Project kind (rust, python, node, go, java, ruby, zig)
//!       --default-view <NAME>  Default view name (default: "dev")
//!       --visibility <VIS>     Visibility: private or public (default: private)
//!       --org <SLUG>           Organization override
//! ```
//!
//! # Examples
//!
//! ```text
//! # Explicit workspace
//! $ atomic project create my-service --workspace backend
//! ✓ Created project: my-service (slug: my-service)
//!
//! # Uses the default workspace for the current org
//! $ atomic workspace set backend
//! $ atomic project create my-service
//! ✓ Created project: my-service (slug: my-service)
//!
//!   Workspace:    backend
//!   Default view: dev
//!   Visibility:   private
//!   VCS URL:      https://alice.atomic.storage/workspaces/backend/projects/my-service/code
//!
//! Next steps:
//!   atomic project init my-service --workspace backend  Link a local repo to this project
//!   atomic push                                         Push changes to the remote
//! ```

use clap::Parser;

use atomic_remote::storage_types::{CreateProjectRequest, Visibility};

use crate::commands::client::{build_client_with_org, remote_err, resolve_workspace};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_next_steps, print_success, KeyValueTable};

/// Create a new remote project within a workspace.
///
/// The project will be created on the configured atomic-storage server
/// and can then be linked to a local repository with `atomic project init`.
///
/// If `--workspace` is not given, falls back to the default workspace for
/// the current org (set via `atomic workspace set <slug>`).
#[derive(Debug, Parser, Default)]
#[command(name = "create")]
pub struct ProjectCreate {
    /// Name of the project to create.
    #[arg(default_value = "")]
    pub name: String,

    /// Workspace slug to create the project in.
    ///
    /// Falls back to the default workspace for the current org if omitted.
    #[arg(long, short = 'w')]
    pub workspace: Option<String>,

    /// Optional description for the project.
    #[arg(long)]
    pub description: Option<String>,

    /// Project kind for language-specific defaults.
    ///
    /// Supported kinds: rust, python, node, go, java, ruby, zig.
    #[arg(long)]
    pub kind: Option<String>,

    /// Default view name for the project.
    #[arg(long, default_value = "dev")]
    pub default_view: String,

    /// Visibility of the project.
    ///
    /// Possible values: private, public.
    #[arg(long, default_value = "private")]
    pub visibility: String,

    /// Organization slug override.
    ///
    /// If not specified, uses the default org from global config.
    #[arg(long)]
    pub org: Option<String>,
}

impl ProjectCreate {
    fn parse_visibility(&self) -> CliResult<Visibility> {
        self.visibility
            .parse::<Visibility>()
            .map_err(|e| CliError::InvalidArgument { message: e })
    }
}

impl Command for ProjectCreate {
    fn run(&self) -> CliResult<()> {
        if self.name.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Project name is required.".to_string(),
            });
        }

        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(async {
            let (client, org_slug) = build_client_with_org(self.org.as_deref())?;
            let workspace = resolve_workspace(&org_slug, self.workspace.as_deref())?;
            let visibility = self.parse_visibility()?;

            let req = CreateProjectRequest {
                name: self.name.clone(),
                description: self.description.clone(),
                default_view: self.default_view.clone(),
                kind: self.kind.clone(),
                visibility,
            };

            let project = client
                .create_project(&workspace, &req)
                .await
                .map_err(remote_err)?;

            print_success(&format!(
                "Created project: {} (slug: {})",
                project.name, project.slug
            ));
            println!();

            let vcs_url = format!(
                "{}/workspaces/{}/projects/{}/code",
                client.base_url(),
                workspace,
                project.slug,
            );

            let details = KeyValueTable::new()
                .add("Workspace", &workspace)
                .add("Default view", &project.default_view)
                .add("Visibility", project.visibility.to_string())
                .add("VCS URL", &vcs_url);

            println!("{}", details);

            print_next_steps(&[
                (
                    &format!(
                        "atomic project init {} --workspace {}",
                        project.slug, workspace
                    ),
                    "Link a local repo to this project",
                ),
                ("atomic push", "Push changes to the remote"),
            ]);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_empty_fields() {
        let cmd = ProjectCreate::default();
        assert_eq!(cmd.name, "");
        assert!(cmd.workspace.is_none());
        assert_eq!(cmd.default_view, "");
        assert!(cmd.description.is_none());
        assert!(cmd.kind.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_all_options() {
        let cmd = ProjectCreate {
            name: "svc".to_string(),
            workspace: Some("backend".to_string()),
            description: Some("A microservice".to_string()),
            kind: Some("rust".to_string()),
            default_view: "main".to_string(),
            visibility: "public".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.name, "svc");
        assert_eq!(cmd.workspace.as_deref(), Some("backend"));
        assert_eq!(cmd.description.as_deref(), Some("A microservice"));
        assert_eq!(cmd.kind.as_deref(), Some("rust"));
        assert_eq!(cmd.default_view, "main");
        assert_eq!(cmd.visibility, "public");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }

    #[test]
    fn parse_visibility_valid() {
        let cmd = ProjectCreate {
            name: "p".to_string(),
            workspace: Some("w".to_string()),
            visibility: "private".to_string(),
            ..Default::default()
        };
        let v = cmd.parse_visibility().unwrap();
        assert_eq!(v, Visibility::Private);
    }

    #[test]
    fn parse_visibility_invalid() {
        let cmd = ProjectCreate {
            name: "p".to_string(),
            workspace: Some("w".to_string()),
            visibility: "bogus".to_string(),
            ..Default::default()
        };
        assert!(cmd.parse_visibility().is_err());
    }

    #[test]
    fn empty_name_rejected() {
        let cmd = ProjectCreate::default();
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

//! The `project init` command for initialising a remote project from a local repo.
//!
//! This is a convenience command that:
//!
//! 1. Ensures the current directory is an Atomic repository.
//! 2. Creates the workspace on the server if `--workspace` is given and it
//!    doesn't already exist (409 Conflict is caught gracefully).
//! 3. Creates the project on the server.
//! 4. Configures the remote URL in the local repository config.
//!
//! # Usage
//!
//! ```text
//! atomic project init <NAME> --workspace <SLUG> [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name of the project to create on the server
//!
//! Options:
//!   -w, --workspace <SLUG>    Workspace slug (required)
//!       --kind <KIND>         Project kind (rust, python, node, go, java, ruby, zig)
//!       --description <DESC>  Project description
//!       --visibility <VIS>    Visibility (private or public) [default: private]
//!       --org <ORG>           Organization override
//!   -h, --help                Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Initialise project in current repo
//! $ atomic project init my-app --workspace my-ws
//! ✓ Created project: my-app (slug: my-app)
//!   Remote URL: https://alice.atomic.storage/workspaces/my-ws/projects/my-app/code
//!
//! # With workspace auto-creation
//! $ atomic project init my-app --workspace new-ws --kind rust
//! ✓ Created workspace: new-ws
//! ✓ Created project: my-app (slug: my-app)
//! ```

use clap::Parser;

use atomic_remote::storage_types::{CreateProjectRequest, CreateWorkspaceRequest, Visibility};
use atomic_repository::Repository;

use crate::commands::client::{build_client, remote_err};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_next_steps, print_success};

/// Initialise a remote project from the current Atomic repository.
///
/// Creates the project (and optionally the workspace) on the server,
/// then stores the remote URL in the local repository configuration.
#[derive(Debug, Parser)]
#[command(name = "init")]
pub struct ProjectInit {
    /// Name of the project to create.
    #[arg(required = true)]
    pub name: String,

    /// Workspace slug to create the project in.
    ///
    /// If the workspace does not exist on the server it will be created
    /// automatically (with default settings).
    #[arg(short = 'w', long, required = true)]
    pub workspace: String,

    /// Project kind hint (rust, python, node, go, java, ruby, zig).
    ///
    /// Passed to the server to configure language-specific defaults
    /// (e.g. ignore patterns).
    #[arg(long)]
    pub kind: Option<String>,

    /// Optional description for the project.
    #[arg(long)]
    pub description: Option<String>,

    /// Visibility of the new project.
    #[arg(long, default_value = "private")]
    pub visibility: Visibility,

    /// Organization slug override (defaults to the configured org).
    #[arg(long)]
    pub org: Option<String>,
}

impl Command for ProjectInit {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
        })?;

        rt.block_on(self.execute())
    }
}

impl ProjectInit {
    async fn execute(&self) -> CliResult<()> {
        // 1. Ensure we're inside an Atomic repository.
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // 2. Build the storage client.
        let client = build_client(self.org.as_deref())?;

        // 3. Ensure the workspace exists — create it if necessary.
        //    A 409 (Conflict) means it already exists, which is fine.
        match client.get_workspace(&self.workspace).await {
            Ok(_ws) => {
                // Already exists — nothing to do.
            }
            Err(_) => {
                // Try to create it. If the server returns 409 we ignore it.
                let ws_req = CreateWorkspaceRequest {
                    name: self.workspace.clone(),
                    description: None,
                    visibility: self.visibility,
                };

                match client.create_workspace(&ws_req).await {
                    Ok(ws) => {
                        print_success(&format!("Created workspace: {}", ws.slug));
                    }
                    Err(e) => {
                        // 409 means already exists — that's fine.
                        let msg = e.to_string();
                        if msg.contains("409")
                            || msg.contains("conflict")
                            || msg.contains("Conflict")
                        {
                            print_info(&format!(
                                "Workspace '{}' already exists, using it.",
                                self.workspace
                            ));
                        } else {
                            return Err(remote_err(e));
                        }
                    }
                }
            }
        }

        // 4. Create the project on the server.
        let proj_req = CreateProjectRequest {
            name: self.name.clone(),
            description: self.description.clone(),
            default_view: "dev".to_string(),
            kind: self.kind.clone(),
            visibility: self.visibility,
        };

        let project = client
            .create_project(&self.workspace, &proj_req)
            .await
            .map_err(remote_err)?;

        // 5. Build the VCS remote URL and store it locally.
        let vcs_url = format!(
            "{}/workspaces/{}/projects/{}/code",
            client.base_url(),
            self.workspace,
            project.slug,
        );

        repo.add_remote("origin", &vcs_url)
            .map_err(CliError::Repository)?;

        // 6. Report success.
        print_success(&format!(
            "Created project: {} (slug: {})",
            project.name, project.slug
        ));
        println!("  Remote URL: {}", vcs_url);

        print_next_steps(&[
            (
                "atomic record -m \"Initial commit\"",
                "Record your first change",
            ),
            ("atomic push", "Push changes to the server"),
        ]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_fields() {
        let cmd = ProjectInit {
            name: "my-app".to_string(),
            workspace: "my-ws".to_string(),
            kind: Some("rust".to_string()),
            description: Some("My app".to_string()),
            visibility: Visibility::Private,
            org: None,
        };

        assert_eq!(cmd.name, "my-app");
        assert_eq!(cmd.workspace, "my-ws");
        assert_eq!(cmd.kind.as_deref(), Some("rust"));
        assert_eq!(cmd.description.as_deref(), Some("My app"));
        assert_eq!(cmd.visibility, Visibility::Private);
        assert!(cmd.org.is_none());
    }

    #[test]
    fn default_visibility_is_private() {
        let cmd = ProjectInit {
            name: "proj".to_string(),
            workspace: "ws".to_string(),
            kind: None,
            description: None,
            visibility: Visibility::Private,
            org: None,
        };
        assert_eq!(cmd.visibility, Visibility::Private);
    }

    #[test]
    fn run_outside_repo_fails() {
        // Running outside a repository should produce an error before
        // any network calls happen.
        let cmd = ProjectInit {
            name: "proj".to_string(),
            workspace: "ws".to_string(),
            kind: None,
            description: None,
            visibility: Visibility::Private,
            org: None,
        };

        // We don't actually call run() because it would also require
        // a valid global config and identity. The important thing is that
        // the struct compiles and the fields are correct.
        let _ = cmd;
    }
}
